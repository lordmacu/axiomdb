//! B+ Tree persistente con Copy-on-Write y raíz atómica.
//!
//! ## Invariantes
//! - Nodo interno con `n` keys tiene `n+1` children.
//! - Para todo `i`: `children[i]` contiene keys `>= keys[i-1]` y `< keys[i]`.
//! - Las hojas están enlazadas en orden ascendente por `next_leaf`
//!   (aunque el iterador usa traversal del árbol para evitar punteros obsoletos en CoW).
//! - Cada write crea páginas nuevas antes de liberar las antiguas.
//! - `root_pid` se actualiza con `AtomicU64::store(Release)` al fin de cada mutación.

use std::sync::atomic::{AtomicU64, Ordering};

use nexusdb_core::{error::DbError, RecordId};
use nexusdb_storage::{Page, PageType, StorageEngine};

use crate::iter::RangeIter;
use crate::page_layout::{
    cast_internal, cast_internal_mut, cast_leaf, cast_leaf_mut, InternalNodePage, LeafNodePage,
    MAX_KEY_LEN, MIN_KEYS_INTERNAL, MIN_KEYS_LEAF, NULL_PAGE, ORDER_INTERNAL, ORDER_LEAF,
};

// ── Tipos internos ───────────────────────────────────────────────────────────

enum InsertResult {
    Ok(u64),
    Split {
        left_pid: u64,
        right_pid: u64,
        sep: Vec<u8>,
    },
}

enum DeleteResult {
    NotFound,
    Deleted { new_pid: u64, underfull: bool },
}

/// Versión copiada de un nodo leído de página (libera el borrow de storage).
enum NodeCopy {
    Leaf(LeafNodePage),
    Internal(InternalNodePage),
}

impl NodeCopy {
    fn read(storage: &dyn StorageEngine, pid: u64) -> Result<Self, DbError> {
        let page = storage.read_page(pid)?;
        if page.body()[0] == 1 {
            Ok(Self::Leaf(*cast_leaf(page)))
        } else {
            Ok(Self::Internal(*cast_internal(page)))
        }
    }
}

// ── BTree ────────────────────────────────────────────────────────────────────

/// B+ Tree persistente sobre un `StorageEngine`.
pub struct BTree {
    storage: Box<dyn StorageEngine>,
    root_pid: AtomicU64,
}

impl BTree {
    /// Crea o reabre un B+ Tree.
    pub fn new(
        mut storage: Box<dyn StorageEngine>,
        root_page_id: Option<u64>,
    ) -> Result<Self, DbError> {
        let root_pid = match root_page_id {
            Some(pid) => pid,
            None => {
                let pid = storage.alloc_page(PageType::Index)?;
                let mut page = Page::new(PageType::Index, pid);
                let leaf = cast_leaf_mut(&mut page);
                leaf.is_leaf = 1;
                leaf.set_num_keys(0);
                leaf.set_next_leaf(NULL_PAGE);
                page.update_checksum();
                storage.write_page(pid, &page)?;
                pid
            }
        };
        Ok(Self {
            storage,
            root_pid: AtomicU64::new(root_pid),
        })
    }

    pub fn root_page_id(&self) -> u64 {
        self.root_pid.load(Ordering::Acquire)
    }

    // ── Lookup ───────────────────────────────────────────────────────────────

    pub fn lookup(&self, key: &[u8]) -> Result<Option<RecordId>, DbError> {
        Self::check_key(key)?;
        let mut pid = self.root_pid.load(Ordering::Acquire);
        loop {
            match NodeCopy::read(self.storage.as_ref(), pid)? {
                NodeCopy::Leaf(node) => {
                    return Ok(node.search(key).ok().map(|i| node.rid_at(i)));
                }
                NodeCopy::Internal(node) => {
                    let idx = node.find_child_idx(key);
                    pid = node.child_at(idx);
                }
            }
        }
    }

    // ── Insert ───────────────────────────────────────────────────────────────

    pub fn insert(&mut self, key: &[u8], rid: RecordId) -> Result<(), DbError> {
        Self::check_key(key)?;
        let root = self.root_pid.load(Ordering::Acquire);
        match Self::insert_subtree(self.storage.as_mut(), root, key, rid)? {
            InsertResult::Ok(new_root) => {
                // CAS garantiza que si en Fase 7 hubiera otro writer concurrente,
                // el segundo fallaría en lugar de sobrescribir silenciosamente.
                // Con &mut self (Fase 2) siempre tiene éxito — el patrón queda listo.
                self.root_pid
                    .compare_exchange(root, new_root, Ordering::AcqRel, Ordering::Acquire)
                    .map_err(|_| DbError::BTreeCorrupted {
                        msg: "root modificado concurrentemente durante insert".into(),
                    })?;
            }
            InsertResult::Split {
                left_pid,
                right_pid,
                sep,
            } => {
                let new_root = Self::alloc_root(self.storage.as_mut(), &sep, left_pid, right_pid)?;
                self.root_pid
                    .compare_exchange(root, new_root, Ordering::AcqRel, Ordering::Acquire)
                    .map_err(|_| DbError::BTreeCorrupted {
                        msg: "root modificado concurrentemente durante insert (split)".into(),
                    })?;
            }
        }
        Ok(())
    }

    fn insert_subtree(
        storage: &mut dyn StorageEngine,
        pid: u64,
        key: &[u8],
        rid: RecordId,
    ) -> Result<InsertResult, DbError> {
        match NodeCopy::read(storage, pid)? {
            NodeCopy::Leaf(node) => Self::insert_leaf(storage, pid, node, key, rid),
            NodeCopy::Internal(node) => {
                let n = node.num_keys();
                let child_idx = node.find_child_idx(key);
                let child_pid = node.child_at(child_idx);

                match Self::insert_subtree(storage, child_pid, key, rid)? {
                    InsertResult::Ok(new_child_pid) => {
                        // Si el hijo se actualizó in-place (mismo pid), el padre no cambió:
                        // no hay que reescribirlo ni actualizar su puntero child.
                        if new_child_pid == child_pid {
                            return Ok(InsertResult::Ok(pid));
                        }
                        let new_pid = Self::in_place_update_child(
                            storage,
                            pid,
                            node,
                            child_idx,
                            new_child_pid,
                        )?;
                        Ok(InsertResult::Ok(new_pid))
                    }
                    InsertResult::Split {
                        left_pid,
                        right_pid,
                        sep,
                    } => {
                        let mut node2 = node;
                        node2.set_child_at(child_idx, left_pid);

                        if n < ORDER_INTERNAL {
                            node2.insert_at(child_idx, &sep, right_pid);
                            let new_pid = storage.alloc_page(PageType::Index)?;
                            let mut p = Page::new(PageType::Index, new_pid);
                            *cast_internal_mut(&mut p) = node2;
                            p.update_checksum();
                            storage.write_page(new_pid, &p)?;
                            storage.free_page(pid)?;
                            Ok(InsertResult::Ok(new_pid))
                        } else {
                            Self::split_internal(storage, pid, node2, child_idx, &sep, right_pid)
                        }
                    }
                }
            }
        }
    }

    fn insert_leaf(
        storage: &mut dyn StorageEngine,
        old_pid: u64,
        node: LeafNodePage,
        key: &[u8],
        rid: RecordId,
    ) -> Result<InsertResult, DbError> {
        let ins_pos = match node.search(key) {
            Ok(_) => return Err(DbError::DuplicateKey),
            Err(pos) => pos,
        };

        if node.num_keys() < ORDER_LEAF {
            // In-place: con &mut self no hay lectores concurrentes, no necesitamos CoW.
            let mut p = Page::new(PageType::Index, old_pid);
            let n = cast_leaf_mut(&mut p);
            *n = node;
            n.insert_at(ins_pos, key, rid);
            p.update_checksum();
            storage.write_page(old_pid, &p)?;
            return Ok(InsertResult::Ok(old_pid));
        }

        // Split
        let count = node.num_keys();
        let mut kl = [0u8; ORDER_LEAF + 1];
        let mut ks = [[0u8; MAX_KEY_LEN]; ORDER_LEAF + 1];
        let mut rs = [[0u8; 10]; ORDER_LEAF + 1];

        kl[..ins_pos].copy_from_slice(&node.key_lens[..ins_pos]);
        ks[..ins_pos].copy_from_slice(&node.keys[..ins_pos]);
        rs[..ins_pos].copy_from_slice(&node.rids[..ins_pos]);
        kl[ins_pos] = key.len() as u8;
        ks[ins_pos][..key.len()].copy_from_slice(key);
        rs[ins_pos] = crate::page_layout::encode_rid(rid);
        kl[ins_pos + 1..=count].copy_from_slice(&node.key_lens[ins_pos..count]);
        ks[ins_pos + 1..=count].copy_from_slice(&node.keys[ins_pos..count]);
        rs[ins_pos + 1..=count].copy_from_slice(&node.rids[ins_pos..count]);

        let total = count + 1;
        let mid = total / 2;
        let sep = ks[mid][..kl[mid] as usize].to_vec();

        let left_pid = storage.alloc_page(PageType::Index)?;
        let right_pid = storage.alloc_page(PageType::Index)?;

        {
            let mut p = Page::new(PageType::Index, left_pid);
            let ln = cast_leaf_mut(&mut p);
            ln.is_leaf = 1;
            ln.set_num_keys(mid);
            ln.set_next_leaf(right_pid);
            ln.key_lens[..mid].copy_from_slice(&kl[..mid]);
            ln.keys[..mid].copy_from_slice(&ks[..mid]);
            ln.rids[..mid].copy_from_slice(&rs[..mid]);
            p.update_checksum();
            storage.write_page(left_pid, &p)?;
        }
        {
            let rcount = total - mid;
            let mut p = Page::new(PageType::Index, right_pid);
            let rn = cast_leaf_mut(&mut p);
            rn.is_leaf = 1;
            rn.set_num_keys(rcount);
            rn.set_next_leaf(node.next_leaf_val());
            rn.key_lens[..rcount].copy_from_slice(&kl[mid..total]);
            rn.keys[..rcount].copy_from_slice(&ks[mid..total]);
            rn.rids[..rcount].copy_from_slice(&rs[mid..total]);
            p.update_checksum();
            storage.write_page(right_pid, &p)?;
        }

        storage.free_page(old_pid)?;
        Ok(InsertResult::Split {
            left_pid,
            right_pid,
            sep,
        })
    }

    fn split_internal(
        storage: &mut dyn StorageEngine,
        old_pid: u64,
        node: InternalNodePage,
        child_idx: usize,
        sep: &[u8],
        right_child: u64,
    ) -> Result<InsertResult, DbError> {
        let n = node.num_keys();
        let mut kl = [0u8; ORDER_INTERNAL + 1];
        let mut ks = [[0u8; MAX_KEY_LEN]; ORDER_INTERNAL + 1];
        let mut ch = [0u64; ORDER_INTERNAL + 2];

        kl[..n].copy_from_slice(&node.key_lens[..n]);
        ks[..n].copy_from_slice(&node.keys[..n]);
        for (i, c) in ch[..=n].iter_mut().enumerate() {
            *c = node.child_at(i);
        }

        // Insertar sep en child_idx: desplazar [child_idx..n] una posición a la derecha.
        // copy_within maneja correctamente child_idx == n (rango vacío → no-op).
        kl.copy_within(child_idx..n, child_idx + 1);
        ks.copy_within(child_idx..n, child_idx + 1);
        // children: desplazar [child_idx+1..=n] una posición a la derecha
        ch.copy_within(child_idx + 1..=n, child_idx + 2);
        kl[child_idx] = sep.len() as u8;
        ks[child_idx].fill(0);
        ks[child_idx][..sep.len()].copy_from_slice(sep);
        ch[child_idx + 1] = right_child;

        let total = n + 1;
        let mid = total / 2;

        let new_sep = ks[mid][..kl[mid] as usize].to_vec();

        let left_pid = storage.alloc_page(PageType::Index)?;
        let right_pid = storage.alloc_page(PageType::Index)?;

        {
            let mut p = Page::new(PageType::Index, left_pid);
            let ln = cast_internal_mut(&mut p);
            ln.is_leaf = 0;
            ln.set_num_keys(mid);
            ln.key_lens[..mid].copy_from_slice(&kl[..mid]);
            ln.keys[..mid].copy_from_slice(&ks[..mid]);
            for (i, c) in ch[..=mid].iter().enumerate() {
                ln.set_child_at(i, *c);
            }
            p.update_checksum();
            storage.write_page(left_pid, &p)?;
        }

        let right_count = total - mid - 1;
        {
            let mut p = Page::new(PageType::Index, right_pid);
            let rn = cast_internal_mut(&mut p);
            rn.is_leaf = 0;
            rn.set_num_keys(right_count);
            rn.key_lens[..right_count].copy_from_slice(&kl[mid + 1..total]);
            rn.keys[..right_count].copy_from_slice(&ks[mid + 1..total]);
            for (i, c) in ch[mid + 1..=total].iter().enumerate() {
                rn.set_child_at(i, *c);
            }
            p.update_checksum();
            storage.write_page(right_pid, &p)?;
        }

        storage.free_page(old_pid)?;
        Ok(InsertResult::Split {
            left_pid,
            right_pid,
            sep: new_sep,
        })
    }

    fn alloc_root(
        storage: &mut dyn StorageEngine,
        sep: &[u8],
        left_pid: u64,
        right_pid: u64,
    ) -> Result<u64, DbError> {
        let pid = storage.alloc_page(PageType::Index)?;
        let mut p = Page::new(PageType::Index, pid);
        let n = cast_internal_mut(&mut p);
        n.is_leaf = 0;
        n.set_num_keys(1);
        n.set_child_at(0, left_pid);
        n.set_child_at(1, right_pid);
        n.key_lens[0] = sep.len() as u8;
        n.keys[0][..sep.len()].copy_from_slice(sep);
        p.update_checksum();
        storage.write_page(pid, &p)?;
        Ok(pid)
    }

    fn in_place_update_child(
        storage: &mut dyn StorageEngine,
        old_pid: u64,
        mut node: InternalNodePage,
        child_idx: usize,
        new_child: u64,
    ) -> Result<u64, DbError> {
        // In-place: con &mut self no hay lectores concurrentes, no necesitamos CoW.
        node.set_child_at(child_idx, new_child);
        let mut p = Page::new(PageType::Index, old_pid);
        *cast_internal_mut(&mut p) = node;
        p.update_checksum();
        storage.write_page(old_pid, &p)?;
        Ok(old_pid)
    }

    // ── Delete ───────────────────────────────────────────────────────────────

    pub fn delete(&mut self, key: &[u8]) -> Result<bool, DbError> {
        Self::check_key(key)?;
        let root = self.root_pid.load(Ordering::Acquire);
        match Self::delete_subtree(self.storage.as_mut(), root, key, true)? {
            DeleteResult::NotFound => Ok(false),
            DeleteResult::Deleted { new_pid, .. } => {
                let final_root = Self::collapse_root(self.storage.as_mut(), new_pid)?;
                self.root_pid
                    .compare_exchange(root, final_root, Ordering::AcqRel, Ordering::Acquire)
                    .map_err(|_| DbError::BTreeCorrupted {
                        msg: "root modificado concurrentemente durante delete".into(),
                    })?;
                Ok(true)
            }
        }
    }

    fn delete_subtree(
        storage: &mut dyn StorageEngine,
        pid: u64,
        key: &[u8],
        is_root: bool,
    ) -> Result<DeleteResult, DbError> {
        match NodeCopy::read(storage, pid)? {
            NodeCopy::Leaf(node) => Self::delete_leaf(storage, pid, node, key, is_root),
            NodeCopy::Internal(node) => {
                let child_idx = node.find_child_idx(key);
                let child_pid = node.child_at(child_idx);

                match Self::delete_subtree(storage, child_pid, key, false)? {
                    DeleteResult::NotFound => Ok(DeleteResult::NotFound),
                    DeleteResult::Deleted {
                        new_pid: new_child_pid,
                        underfull,
                    } => {
                        if !underfull {
                            let new_pid = Self::in_place_update_child(
                                storage,
                                pid,
                                node,
                                child_idx,
                                new_child_pid,
                            )?;
                            let underfull2 =
                                !is_root && Self::internal_underfull(storage, new_pid)?;
                            Ok(DeleteResult::Deleted {
                                new_pid,
                                underfull: underfull2,
                            })
                        } else {
                            let new_pid =
                                Self::rebalance(storage, pid, node, child_idx, new_child_pid)?;
                            let underfull2 =
                                !is_root && Self::internal_underfull(storage, new_pid)?;
                            Ok(DeleteResult::Deleted {
                                new_pid,
                                underfull: underfull2,
                            })
                        }
                    }
                }
            }
        }
    }

    fn delete_leaf(
        storage: &mut dyn StorageEngine,
        old_pid: u64,
        mut node: LeafNodePage,
        key: &[u8],
        is_root: bool,
    ) -> Result<DeleteResult, DbError> {
        let idx = match node.search(key) {
            Ok(i) => i,
            Err(_) => return Ok(DeleteResult::NotFound),
        };
        node.remove_at(idx);

        let new_pid = storage.alloc_page(PageType::Index)?;
        let mut p = Page::new(PageType::Index, new_pid);
        *cast_leaf_mut(&mut p) = node;
        p.update_checksum();
        storage.write_page(new_pid, &p)?;
        storage.free_page(old_pid)?;

        let underfull = !is_root && node.num_keys() < MIN_KEYS_LEAF;
        Ok(DeleteResult::Deleted { new_pid, underfull })
    }

    fn rebalance(
        storage: &mut dyn StorageEngine,
        parent_pid: u64,
        parent: InternalNodePage,
        child_idx: usize,
        child_pid: u64,
    ) -> Result<u64, DbError> {
        let n = parent.num_keys();
        let child_is_leaf = {
            let p = storage.read_page(child_pid)?;
            p.body()[0] == 1
        };
        let min_keys = if child_is_leaf {
            MIN_KEYS_LEAF
        } else {
            MIN_KEYS_INTERNAL
        };

        if child_idx > 0 {
            let left_pid = parent.child_at(child_idx - 1);
            if Self::node_key_count(storage, left_pid)? > min_keys {
                return Self::rotate_right(
                    storage,
                    parent_pid,
                    parent,
                    child_idx,
                    child_pid,
                    left_pid,
                    child_is_leaf,
                );
            }
        }
        if child_idx < n {
            let right_pid = parent.child_at(child_idx + 1);
            if Self::node_key_count(storage, right_pid)? > min_keys {
                return Self::rotate_left(
                    storage,
                    parent_pid,
                    parent,
                    child_idx,
                    child_pid,
                    right_pid,
                    child_is_leaf,
                );
            }
        }
        if child_idx > 0 {
            let left_pid = parent.child_at(child_idx - 1);
            return Self::merge_children(
                storage,
                parent_pid,
                parent,
                child_idx - 1,
                left_pid,
                child_pid,
                child_is_leaf,
            );
        }
        let right_pid = parent.child_at(child_idx + 1);
        Self::merge_children(
            storage,
            parent_pid,
            parent,
            child_idx,
            child_pid,
            right_pid,
            child_is_leaf,
        )
    }

    fn rotate_right(
        storage: &mut dyn StorageEngine,
        parent_pid: u64,
        mut parent: InternalNodePage,
        child_idx: usize,
        child_pid: u64,
        left_pid: u64,
        is_leaf: bool,
    ) -> Result<u64, DbError> {
        let np = storage.alloc_page(PageType::Index)?;
        let nc = storage.alloc_page(PageType::Index)?;
        let nl = storage.alloc_page(PageType::Index)?;

        if is_leaf {
            let mut left = match NodeCopy::read(storage, left_pid)? {
                NodeCopy::Leaf(n) => n,
                NodeCopy::Internal(_) => unreachable!(),
            };
            let mut child = match NodeCopy::read(storage, child_pid)? {
                NodeCopy::Leaf(n) => n,
                NodeCopy::Internal(_) => unreachable!(),
            };
            let lk = left.num_keys() - 1;
            let bk_len = left.key_lens[lk];
            let bk = left.keys[lk];
            let br = left.rids[lk];
            left.remove_at(lk);
            child.insert_at(
                0,
                &bk[..bk_len as usize],
                crate::page_layout::decode_rid(br),
            );

            parent.key_lens[child_idx - 1] = child.key_lens[0];
            parent.keys[child_idx - 1] = child.keys[0];
            parent.set_child_at(child_idx - 1, nl);
            parent.set_child_at(child_idx, nc);

            let mut lp = Page::new(PageType::Index, nl);
            *cast_leaf_mut(&mut lp) = left;
            lp.update_checksum();
            storage.write_page(nl, &lp)?;
            let mut cp = Page::new(PageType::Index, nc);
            *cast_leaf_mut(&mut cp) = child;
            cp.update_checksum();
            storage.write_page(nc, &cp)?;
        } else {
            let mut left = match NodeCopy::read(storage, left_pid)? {
                NodeCopy::Internal(n) => n,
                NodeCopy::Leaf(_) => unreachable!(),
            };
            let mut child = match NodeCopy::read(storage, child_pid)? {
                NodeCopy::Internal(n) => n,
                NodeCopy::Leaf(_) => unreachable!(),
            };
            let lk = left.num_keys() - 1;
            let sep_len = parent.key_lens[child_idx - 1];
            let sep_key = parent.keys[child_idx - 1];
            let last_ch = left.child_at(lk + 1);

            let cn = child.num_keys();
            child.key_lens[..cn].rotate_right(1);
            child.keys[..cn].rotate_right(1);
            for i in (0..=cn).rev() {
                let p = child.child_at(i);
                child.set_child_at(i + 1, p);
            }
            child.key_lens[0] = sep_len;
            child.keys[0] = sep_key;
            child.set_child_at(0, last_ch);
            child.set_num_keys(cn + 1);

            parent.key_lens[child_idx - 1] = left.key_lens[lk];
            parent.keys[child_idx - 1] = left.keys[lk];
            left.key_lens[lk] = 0;
            left.keys[lk].fill(0);
            left.set_num_keys(lk);

            parent.set_child_at(child_idx - 1, nl);
            parent.set_child_at(child_idx, nc);

            let mut lp = Page::new(PageType::Index, nl);
            *cast_internal_mut(&mut lp) = left;
            lp.update_checksum();
            storage.write_page(nl, &lp)?;
            let mut cp = Page::new(PageType::Index, nc);
            *cast_internal_mut(&mut cp) = child;
            cp.update_checksum();
            storage.write_page(nc, &cp)?;
        }

        let mut pp = Page::new(PageType::Index, np);
        *cast_internal_mut(&mut pp) = parent;
        pp.update_checksum();
        storage.write_page(np, &pp)?;

        storage.free_page(parent_pid)?;
        storage.free_page(child_pid)?;
        storage.free_page(left_pid)?;
        Ok(np)
    }

    fn rotate_left(
        storage: &mut dyn StorageEngine,
        parent_pid: u64,
        mut parent: InternalNodePage,
        child_idx: usize,
        child_pid: u64,
        right_pid: u64,
        is_leaf: bool,
    ) -> Result<u64, DbError> {
        let np = storage.alloc_page(PageType::Index)?;
        let nc = storage.alloc_page(PageType::Index)?;
        let nr = storage.alloc_page(PageType::Index)?;

        if is_leaf {
            let mut child = match NodeCopy::read(storage, child_pid)? {
                NodeCopy::Leaf(n) => n,
                NodeCopy::Internal(_) => unreachable!(),
            };
            let mut right = match NodeCopy::read(storage, right_pid)? {
                NodeCopy::Leaf(n) => n,
                NodeCopy::Internal(_) => unreachable!(),
            };
            let bk_len = right.key_lens[0];
            let bk = right.keys[0];
            let br = right.rids[0];
            right.remove_at(0);
            let cn = child.num_keys();
            child.key_lens[cn] = bk_len;
            child.keys[cn] = bk;
            child.rids[cn] = br;
            child.set_num_keys(cn + 1);

            parent.key_lens[child_idx] = right.key_lens[0];
            parent.keys[child_idx] = right.keys[0];
            parent.set_child_at(child_idx, nc);
            parent.set_child_at(child_idx + 1, nr);

            let mut cp = Page::new(PageType::Index, nc);
            *cast_leaf_mut(&mut cp) = child;
            cp.update_checksum();
            storage.write_page(nc, &cp)?;
            let mut rp = Page::new(PageType::Index, nr);
            *cast_leaf_mut(&mut rp) = right;
            rp.update_checksum();
            storage.write_page(nr, &rp)?;
        } else {
            let mut child = match NodeCopy::read(storage, child_pid)? {
                NodeCopy::Internal(n) => n,
                NodeCopy::Leaf(_) => unreachable!(),
            };
            let mut right = match NodeCopy::read(storage, right_pid)? {
                NodeCopy::Internal(n) => n,
                NodeCopy::Leaf(_) => unreachable!(),
            };
            let cn = child.num_keys();
            child.key_lens[cn] = parent.key_lens[child_idx];
            child.keys[cn] = parent.keys[child_idx];
            child.set_child_at(cn + 1, right.child_at(0));
            child.set_num_keys(cn + 1);

            parent.key_lens[child_idx] = right.key_lens[0];
            parent.keys[child_idx] = right.keys[0];
            parent.set_child_at(child_idx, nc);
            parent.set_child_at(child_idx + 1, nr);
            right.remove_at(0, 0);

            let mut cp = Page::new(PageType::Index, nc);
            *cast_internal_mut(&mut cp) = child;
            cp.update_checksum();
            storage.write_page(nc, &cp)?;
            let mut rp = Page::new(PageType::Index, nr);
            *cast_internal_mut(&mut rp) = right;
            rp.update_checksum();
            storage.write_page(nr, &rp)?;
        }

        let mut pp = Page::new(PageType::Index, np);
        *cast_internal_mut(&mut pp) = parent;
        pp.update_checksum();
        storage.write_page(np, &pp)?;

        storage.free_page(parent_pid)?;
        storage.free_page(child_pid)?;
        storage.free_page(right_pid)?;
        Ok(np)
    }

    fn merge_children(
        storage: &mut dyn StorageEngine,
        parent_pid: u64,
        mut parent: InternalNodePage,
        sep_idx: usize,
        left_pid: u64,
        right_pid: u64,
        is_leaf: bool,
    ) -> Result<u64, DbError> {
        let mp = storage.alloc_page(PageType::Index)?;
        let npp = storage.alloc_page(PageType::Index)?;

        if is_leaf {
            let left = match NodeCopy::read(storage, left_pid)? {
                NodeCopy::Leaf(n) => n,
                NodeCopy::Internal(_) => unreachable!(),
            };
            let right = match NodeCopy::read(storage, right_pid)? {
                NodeCopy::Leaf(n) => n,
                NodeCopy::Internal(_) => unreachable!(),
            };
            let mut merged = left;
            let ln = left.num_keys();
            let rn = right.num_keys();
            merged.key_lens[ln..ln + rn].copy_from_slice(&right.key_lens[..rn]);
            merged.keys[ln..ln + rn].copy_from_slice(&right.keys[..rn]);
            merged.rids[ln..ln + rn].copy_from_slice(&right.rids[..rn]);
            merged.set_num_keys(ln + rn);
            merged.set_next_leaf(right.next_leaf_val());

            let mut pg = Page::new(PageType::Index, mp);
            *cast_leaf_mut(&mut pg) = merged;
            pg.update_checksum();
            storage.write_page(mp, &pg)?;
        } else {
            let left = match NodeCopy::read(storage, left_pid)? {
                NodeCopy::Internal(n) => n,
                NodeCopy::Leaf(_) => unreachable!(),
            };
            let right = match NodeCopy::read(storage, right_pid)? {
                NodeCopy::Internal(n) => n,
                NodeCopy::Leaf(_) => unreachable!(),
            };
            let mut merged = left;
            let ln = left.num_keys();
            let rn = right.num_keys();
            merged.key_lens[ln] = parent.key_lens[sep_idx];
            merged.keys[ln] = parent.keys[sep_idx];
            merged.key_lens[ln + 1..ln + 1 + rn].copy_from_slice(&right.key_lens[..rn]);
            merged.keys[ln + 1..ln + 1 + rn].copy_from_slice(&right.keys[..rn]);
            for i in 0..=rn {
                merged.set_child_at(ln + 1 + i, right.child_at(i));
            }
            merged.set_num_keys(ln + 1 + rn);

            let mut pg = Page::new(PageType::Index, mp);
            *cast_internal_mut(&mut pg) = merged;
            pg.update_checksum();
            storage.write_page(mp, &pg)?;
        }

        parent.set_child_at(sep_idx, mp);
        parent.remove_at(sep_idx, sep_idx + 1);

        let mut pp = Page::new(PageType::Index, npp);
        *cast_internal_mut(&mut pp) = parent;
        pp.update_checksum();
        storage.write_page(npp, &pp)?;

        storage.free_page(parent_pid)?;
        storage.free_page(left_pid)?;
        storage.free_page(right_pid)?;
        Ok(npp)
    }

    fn collapse_root(storage: &mut dyn StorageEngine, root_pid: u64) -> Result<u64, DbError> {
        let (is_empty_internal, only_child) = {
            let page = storage.read_page(root_pid)?;
            if page.body()[0] == 0 {
                let node = cast_internal(page);
                if node.num_keys() == 0 {
                    (true, node.child_at(0))
                } else {
                    (false, 0)
                }
            } else {
                (false, 0)
            }
        };
        if is_empty_internal {
            storage.free_page(root_pid)?;
            return Ok(only_child);
        }
        Ok(root_pid)
    }

    // ── Range scan ───────────────────────────────────────────────────────────

    pub fn range<'a>(
        &'a self,
        from: std::ops::Bound<&[u8]>,
        to: std::ops::Bound<&[u8]>,
    ) -> Result<RangeIter<'a>, DbError> {
        use std::ops::Bound::*;

        let root_pid = self.root_pid.load(Ordering::Acquire);
        let start_pid = match from {
            Unbounded => self.leftmost_leaf()?,
            Included(k) | Excluded(k) => self.find_leaf_for(k)?,
        };

        Ok(RangeIter::new(
            self.storage.as_ref(),
            root_pid,
            start_pid,
            from.map(|k| k.to_vec()),
            to.map(|k| k.to_vec()),
        ))
    }

    // ── Helpers ──────────────────────────────────────────────────────────────

    fn check_key(key: &[u8]) -> Result<(), DbError> {
        if key.is_empty() || key.len() > MAX_KEY_LEN {
            return Err(DbError::KeyTooLong {
                len: key.len(),
                max: MAX_KEY_LEN,
            });
        }
        Ok(())
    }

    fn leftmost_leaf(&self) -> Result<u64, DbError> {
        let mut pid = self.root_pid.load(Ordering::Acquire);
        loop {
            match NodeCopy::read(self.storage.as_ref(), pid)? {
                NodeCopy::Leaf(_) => return Ok(pid),
                NodeCopy::Internal(n) => pid = n.child_at(0),
            }
        }
    }

    fn find_leaf_for(&self, key: &[u8]) -> Result<u64, DbError> {
        let mut pid = self.root_pid.load(Ordering::Acquire);
        loop {
            match NodeCopy::read(self.storage.as_ref(), pid)? {
                NodeCopy::Leaf(_) => return Ok(pid),
                NodeCopy::Internal(n) => pid = n.child_at(n.find_child_idx(key)),
            }
        }
    }

    fn node_key_count(storage: &dyn StorageEngine, pid: u64) -> Result<usize, DbError> {
        Ok(match NodeCopy::read(storage, pid)? {
            NodeCopy::Leaf(n) => n.num_keys(),
            NodeCopy::Internal(n) => n.num_keys(),
        })
    }

    fn internal_underfull(storage: &dyn StorageEngine, pid: u64) -> Result<bool, DbError> {
        let page = storage.read_page(pid)?;
        Ok(cast_internal(page).num_keys() < MIN_KEYS_INTERNAL)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexusdb_storage::MemoryStorage;

    fn rid(n: u64) -> RecordId {
        RecordId {
            page_id: n,
            slot_id: 0,
        }
    }

    #[test]
    fn test_insert_lookup_single() {
        let mut tree = BTree::new(Box::new(MemoryStorage::new()), None).unwrap();
        tree.insert(b"hello", rid(1)).unwrap();
        assert_eq!(tree.lookup(b"hello").unwrap(), Some(rid(1)));
        assert_eq!(tree.lookup(b"world").unwrap(), None);
    }

    #[test]
    fn test_duplicate_key_error() {
        let mut tree = BTree::new(Box::new(MemoryStorage::new()), None).unwrap();
        tree.insert(b"key", rid(1)).unwrap();
        assert!(matches!(
            tree.insert(b"key", rid(2)),
            Err(DbError::DuplicateKey)
        ));
    }

    #[test]
    fn test_key_too_long() {
        let mut tree = BTree::new(Box::new(MemoryStorage::new()), None).unwrap();
        let long_key = vec![b'x'; MAX_KEY_LEN + 1];
        assert!(matches!(
            tree.insert(&long_key, rid(1)),
            Err(DbError::KeyTooLong { .. })
        ));
    }

    #[test]
    fn test_insert_forces_leaf_split() {
        let mut tree = BTree::new(Box::new(MemoryStorage::new()), None).unwrap();
        for i in 0..=(ORDER_LEAF as u64) {
            let key = format!("{:08}", i);
            tree.insert(key.as_bytes(), rid(i)).unwrap();
        }
        for i in 0..=(ORDER_LEAF as u64) {
            let key = format!("{:08}", i);
            assert_eq!(tree.lookup(key.as_bytes()).unwrap(), Some(rid(i)));
        }
    }

    #[test]
    fn test_insert_forces_root_split() {
        let mut tree = BTree::new(Box::new(MemoryStorage::new()), None).unwrap();
        let count = ORDER_LEAF * 2 + 5;
        for i in 0..count {
            let key = format!("{:08}", i);
            tree.insert(key.as_bytes(), rid(i as u64)).unwrap();
        }
        for i in 0..count {
            let key = format!("{:08}", i);
            assert_eq!(tree.lookup(key.as_bytes()).unwrap(), Some(rid(i as u64)));
        }
    }

    #[test]
    fn test_delete_existing() {
        let mut tree = BTree::new(Box::new(MemoryStorage::new()), None).unwrap();
        tree.insert(b"aaa", rid(1)).unwrap();
        tree.insert(b"bbb", rid(2)).unwrap();
        tree.insert(b"ccc", rid(3)).unwrap();
        assert!(tree.delete(b"bbb").unwrap());
        assert_eq!(tree.lookup(b"bbb").unwrap(), None);
        assert_eq!(tree.lookup(b"aaa").unwrap(), Some(rid(1)));
        assert_eq!(tree.lookup(b"ccc").unwrap(), Some(rid(3)));
    }

    #[test]
    fn test_delete_nonexistent() {
        let mut tree = BTree::new(Box::new(MemoryStorage::new()), None).unwrap();
        tree.insert(b"aaa", rid(1)).unwrap();
        assert!(!tree.delete(b"zzz").unwrap());
    }

    #[test]
    fn test_insert_delete_1k_sequential() {
        let mut tree = BTree::new(Box::new(MemoryStorage::new()), None).unwrap();
        let count = 1000usize;
        for i in 0..count {
            let key = format!("{:08}", i);
            tree.insert(key.as_bytes(), rid(i as u64)).unwrap();
        }
        for i in (0..count).step_by(2) {
            let key = format!("{:08}", i);
            assert!(tree.delete(key.as_bytes()).unwrap());
        }
        for i in 0..count {
            let key = format!("{:08}", i);
            let expected = if i % 2 == 0 {
                None
            } else {
                Some(rid(i as u64))
            };
            assert_eq!(tree.lookup(key.as_bytes()).unwrap(), expected, "key={key}");
        }
    }
}
