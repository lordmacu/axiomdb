//! Iterador lazy de range scan sobre el B+ Tree.
//!
//! ## Por qué no usamos `next_leaf`
//!
//! Con Copy-on-Write, cada write a una hoja crea un nuevo page_id. La hoja
//! izquierda que apuntaba a `old_leaf_pid` via `next_leaf` queda con un puntero
//! a una página ya liberada. Para evitar este problema, el iterador traversa
//! el árbol desde la raíz cuando necesita avanzar a la siguiente hoja.
//!
//! **Costo**: O(log n) por cruce de frontera entre hojas — aceptable para
//! range scans donde la mayoría del tiempo se consume en las hojas.

use std::ops::Bound;

use nexusdb_core::{error::DbError, RecordId};
use nexusdb_storage::StorageEngine;

use crate::page_layout::{cast_internal, cast_leaf, NULL_PAGE};

/// Iterador lazy de range scan.
///
/// Cada `next()` carga una entrada de la hoja actual.
/// Al agotar una hoja, traversa el árbol para encontrar la siguiente.
pub struct RangeIter<'a> {
    storage: &'a dyn StorageEngine,
    root_pid: u64,
    current_pid: u64,
    slot_idx: usize,
    from: Bound<Vec<u8>>,
    to: Bound<Vec<u8>>,
    last_key: Option<Vec<u8>>, // última key retornada (para buscar siguiente hoja)
    done: bool,
}

impl<'a> RangeIter<'a> {
    pub(crate) fn new(
        storage: &'a dyn StorageEngine,
        root_pid: u64,
        start_pid: u64,
        from: Bound<Vec<u8>>,
        to: Bound<Vec<u8>>,
    ) -> Self {
        Self {
            storage,
            root_pid,
            current_pid: start_pid,
            slot_idx: 0,
            from,
            to,
            last_key: None,
            done: false,
        }
    }

    /// Verifica si `key` está dentro del bound de inicio del rango.
    fn above_lower(&self, key: &[u8]) -> bool {
        match &self.from {
            Bound::Unbounded => true,
            Bound::Included(lo) => key >= lo.as_slice(),
            Bound::Excluded(lo) => key > lo.as_slice(),
        }
    }

    /// Verifica si `key` está dentro del bound de fin del rango.
    fn below_upper(&self, key: &[u8]) -> bool {
        match &self.to {
            Bound::Unbounded => true,
            Bound::Included(hi) => key <= hi.as_slice(),
            Bound::Excluded(hi) => key < hi.as_slice(),
        }
    }

    /// Encuentra la siguiente hoja después de `after_key`.
    ///
    /// Traversa el árbol desde root, desciendo hasta el nodo que contendría
    /// `after_key`, luego sube buscando el primer hermano a la derecha, y
    /// finalmente baja hasta la hoja más a la izquierda de ese subárbol.
    fn find_next_leaf(&self, after_key: &[u8]) -> Result<Option<u64>, DbError> {
        // 1. Descender y guardar el stack con (page_id, next_sibling_idx)
        let mut stack: Vec<(u64, usize)> = Vec::new();
        let mut pid = self.root_pid;

        loop {
            let page = self.storage.read_page(pid)?;
            if page.body()[0] == 1 {
                // Llegamos a la hoja. Salir y buscar el siguiente hermano.
                break;
            }
            let node = cast_internal(page);
            let idx = node.find_child_idx(after_key);
            // Guardar este nodo con el índice del SIGUIENTE hermano (idx+1)
            stack.push((pid, idx + 1));
            pid = node.child_at(idx);
        }

        // 2. Subir hasta encontrar un hermano a la derecha
        loop {
            let Some((parent_pid, next_idx)) = stack.pop() else {
                return Ok(None); // Sin más hojas
            };

            let page = self.storage.read_page(parent_pid)?;
            let node = cast_internal(page);
            if next_idx <= node.num_keys() {
                // Hay un hijo en next_idx → descender hasta la hoja más a la izquierda
                let subtree_root = node.child_at(next_idx);
                return Ok(Some(Self::leftmost_leaf(self.storage, subtree_root)?));
            }
            // Este nodo tampoco tiene más hijos → seguir subiendo
        }
    }

    /// Retorna el page_id de la hoja más a la izquierda del subárbol.
    fn leftmost_leaf(storage: &dyn StorageEngine, pid: u64) -> Result<u64, DbError> {
        let mut pid = pid;
        loop {
            let page = storage.read_page(pid)?;
            if page.body()[0] == 1 {
                return Ok(pid);
            }
            pid = cast_internal(page).child_at(0);
        }
    }
}

impl<'a> Iterator for RangeIter<'a> {
    type Item = Result<(Vec<u8>, RecordId), DbError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }

        loop {
            if self.current_pid == NULL_PAGE {
                self.done = true;
                return None;
            }

            // Leer hoja y buscar el siguiente slot en rango.
            // El bloque asegura que el borrow de `page` expira antes de `find_next_leaf`.
            enum SlotResult {
                Found(Vec<u8>, RecordId),
                Before,
                After,
                Exhausted,
            }

            let result = {
                let page = match self.storage.read_page(self.current_pid) {
                    Ok(p) => p,
                    Err(e) => return Some(Err(e)),
                };
                let node = cast_leaf(page);
                let num = node.num_keys();

                if self.slot_idx >= num {
                    SlotResult::Exhausted
                } else {
                    let key = node.key_at(self.slot_idx).to_vec();
                    let rid = node.rid_at(self.slot_idx);
                    self.slot_idx += 1;

                    if !self.above_lower(&key) {
                        SlotResult::Before
                    } else if !self.below_upper(&key) {
                        SlotResult::After
                    } else {
                        SlotResult::Found(key, rid)
                    }
                }
            };

            match result {
                SlotResult::Before => {
                    // Antes del rango: continuar al siguiente slot
                    continue;
                }
                SlotResult::After => {
                    self.done = true;
                    return None;
                }
                SlotResult::Found(key, rid) => {
                    self.last_key = Some(key.clone());
                    return Some(Ok((key, rid)));
                }
                SlotResult::Exhausted => {
                    // Hoja agotada: buscar la siguiente vía traversal del árbol
                    let after_key = match self.last_key.take() {
                        Some(k) => k,
                        None => {
                            // Hoja vacía o todo era Before, no hay más
                            self.done = true;
                            return None;
                        }
                    };

                    match self.find_next_leaf(&after_key) {
                        Err(e) => {
                            self.done = true;
                            return Some(Err(e));
                        }
                        Ok(None) => {
                            self.done = true;
                            return None;
                        }
                        Ok(Some(next_pid)) => {
                            self.current_pid = next_pid;
                            self.slot_idx = 0;
                            self.last_key = Some(after_key);
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::BTree;
    use nexusdb_core::RecordId;
    use nexusdb_storage::MemoryStorage;

    fn rid(n: u64) -> RecordId {
        RecordId {
            page_id: n,
            slot_id: 0,
        }
    }

    fn build_tree(count: usize) -> BTree {
        let mut tree = BTree::new(Box::new(MemoryStorage::new()), None).unwrap();
        for i in 0..count {
            let key = format!("{:04}", i);
            tree.insert(key.as_bytes(), rid(i as u64)).unwrap();
        }
        tree
    }

    #[test]
    fn test_range_full_scan() {
        let tree = build_tree(100);
        let results: Vec<_> = tree
            .range(Bound::Unbounded, Bound::Unbounded)
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(results.len(), 100);
        for i in 0..99 {
            assert!(
                results[i].0 < results[i + 1].0,
                "no está en orden en índice {i}"
            );
        }
    }

    #[test]
    fn test_range_included_bounds() {
        let tree = build_tree(50);
        let from = b"0010".to_vec();
        let to = b"0020".to_vec();
        let results: Vec<_> = tree
            .range(
                Bound::Included(from.as_slice()),
                Bound::Included(to.as_slice()),
            )
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(results.len(), 11);
        assert_eq!(results.first().unwrap().0, b"0010");
        assert_eq!(results.last().unwrap().0, b"0020");
    }

    #[test]
    fn test_range_excluded_bounds() {
        let tree = build_tree(50);
        let from = b"0010".to_vec();
        let to = b"0020".to_vec();
        let results: Vec<_> = tree
            .range(
                Bound::Excluded(from.as_slice()),
                Bound::Excluded(to.as_slice()),
            )
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(results.len(), 9);
        assert_eq!(results.first().unwrap().0, b"0011");
        assert_eq!(results.last().unwrap().0, b"0019");
    }

    #[test]
    fn test_range_unbounded_start() {
        let tree = build_tree(30);
        let to = b"0009".to_vec();
        let results: Vec<_> = tree
            .range(Bound::Unbounded, Bound::Included(to.as_slice()))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(results.len(), 10);
    }

    #[test]
    fn test_range_unbounded_end() {
        let tree = build_tree(30);
        let from = b"0025".to_vec();
        let results: Vec<_> = tree
            .range(Bound::Included(from.as_slice()), Bound::Unbounded)
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(results.len(), 5);
    }

    #[test]
    fn test_range_empty() {
        let tree = build_tree(10);
        let from = b"0099".to_vec();
        let to = b"0999".to_vec();
        let results: Vec<_> = tree
            .range(
                Bound::Included(from.as_slice()),
                Bound::Included(to.as_slice()),
            )
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert!(results.is_empty());
    }

    #[test]
    fn test_range_spans_multiple_leaves() {
        // Usar suficientes keys para forzar múltiples hojas
        let count = 500;
        let tree = build_tree(count);
        let results: Vec<_> = tree
            .range(Bound::Unbounded, Bound::Unbounded)
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(results.len(), count, "deben retornarse todas las keys");
        for i in 0..results.len() - 1 {
            assert!(
                results[i].0 < results[i + 1].0,
                "no está en orden en índice {i}"
            );
        }
    }
}
