//! Layout de nodos del B+ Tree en páginas de 16 KB.
//!
//! Todos los campos son arrays de `u8` (alignment 1) para garantizar que
//! `bytemuck::Pod` funcione sin padding implícito y sin problemas de alineación.
//! Los valores multibyte se almacenan en little-endian.
//!
//! ## Constantes de layout
//!
//! `PAGE_BODY_SIZE = 16_320` bytes disponibles.
//!
//! ### Nodo interno (`ORDER_INTERNAL = 223`)
//! ```text
//! [header:    8 B]  is_leaf=0 | _pad | num_keys([u8;2]) | _pad([u8;4])
//! [key_lens: 223 B] longitud real de cada key (0 = slot vacío)
//! [children: 1792 B] (223+1) punteros a páginas, 8 bytes c/u
//! [keys:   14272 B] 223 × 64 bytes, zero-padded hasta MAX_KEY_LEN
//! Total: 16295 ≤ 16320 ✓
//! ```
//!
//! ### Nodo hoja (`ORDER_LEAF = 217`)
//! ```text
//! [header:    16 B]  is_leaf=1 | _pad | num_keys([u8;2]) | _pad([u8;4]) | next_leaf([u8;8])
//! [key_lens: 217 B]  longitud real de cada key
//! [rids:    2170 B]  217 × 10 bytes: page_id(8 LE) + slot_id(2 LE)
//! [keys:   13888 B]  217 × 64 bytes, zero-padded
//! Total: 16291 ≤ 16320 ✓
//! ```

use std::mem::size_of;

use nexusdb_core::RecordId;

// ── Constantes públicas ───────────────────────────────────────────────────────

/// Longitud máxima de una key en bytes.
pub const MAX_KEY_LEN: usize = 64;

/// Máximo número de keys en un nodo interno.
pub const ORDER_INTERNAL: usize = 223;

/// Máximo número de keys en un nodo hoja.
pub const ORDER_LEAF: usize = 217;

/// Tamaño del body de una página (PAGE_SIZE - HEADER_SIZE).
pub const PAGE_BODY_SIZE: usize = nexusdb_storage::PAGE_SIZE - nexusdb_storage::HEADER_SIZE;

/// Sentinel: no hay siguiente hoja / no hay child.
pub const NULL_PAGE: u64 = u64::MAX;

/// Mínimo de keys en un nodo interno (excepto raíz).
pub const MIN_KEYS_INTERNAL: usize = ORDER_INTERNAL / 2;

/// Mínimo de keys en un nodo hoja (excepto cuando es también raíz).
pub const MIN_KEYS_LEAF: usize = ORDER_LEAF / 2;

// ── Nodo Interno ─────────────────────────────────────────────────────────────

/// Representación binaria de un nodo interno en el body de una página.
///
/// Todos los campos son arrays de `[u8; N]` → alignment = 1, sin padding implícito.
///
/// Layout (16295 bytes):
/// ```text
/// Offset    Tamaño  Campo
///      0         1  is_leaf  (siempre 0)
///      1         1  _pad0
///      2         2  num_keys  (LE u16)
///      4         4  _pad1
///      8       223  key_lens  (1 byte por key: longitud real)
///    231      1792  children  (224 × [u8;8], LE u64 por entrada)
///   2023     14272  keys      (223 × [u8;64], zero-padded)
/// Total: 16295
/// ```
#[repr(C)]
#[derive(Clone, Copy)]
pub struct InternalNodePage {
    pub is_leaf: u8,
    pub _pad0: u8,
    pub num_keys: [u8; 2],
    pub _pad1: [u8; 4],
    pub key_lens: [u8; ORDER_INTERNAL],
    pub children: [[u8; 8]; ORDER_INTERNAL + 1],
    pub keys: [[u8; MAX_KEY_LEN]; ORDER_INTERNAL],
}

const _: () = assert!(
    size_of::<InternalNodePage>()
        == 8 + ORDER_INTERNAL + (ORDER_INTERNAL + 1) * 8 + ORDER_INTERNAL * MAX_KEY_LEN,
    "InternalNodePage: tamaño incorrecto"
);
const _: () = assert!(
    size_of::<InternalNodePage>() <= PAGE_BODY_SIZE,
    "InternalNodePage no cabe en el body de una página"
);

// SAFETY: InternalNodePage es #[repr(C)] con todos los campos siendo arrays de u8.
// - No hay padding implícito (alignment = 1, todos los campos son u8 o [u8;N]).
// - Cualquier secuencia de bits es un valor válido (todos los campos son u8).
// - El tamaño es exactamente la suma de los campos (verificado por el assert anterior).
unsafe impl bytemuck::Zeroable for InternalNodePage {}
unsafe impl bytemuck::Pod for InternalNodePage {}

impl InternalNodePage {
    pub fn num_keys(&self) -> usize {
        u16::from_le_bytes(self.num_keys) as usize
    }

    pub fn set_num_keys(&mut self, n: usize) {
        self.num_keys = (n as u16).to_le_bytes();
    }

    pub fn key_at(&self, i: usize) -> &[u8] {
        &self.keys[i][..self.key_lens[i] as usize]
    }

    pub fn set_key_at(&mut self, i: usize, k: &[u8]) {
        debug_assert!(k.len() <= MAX_KEY_LEN);
        self.key_lens[i] = k.len() as u8;
        self.keys[i][..k.len()].copy_from_slice(k);
        // Limpiar bytes restantes para evitar datos basura
        self.keys[i][k.len()..].fill(0);
    }

    pub fn child_at(&self, i: usize) -> u64 {
        u64::from_le_bytes(self.children[i])
    }

    pub fn set_child_at(&mut self, i: usize, pid: u64) {
        self.children[i] = pid.to_le_bytes();
    }

    /// Índice del hijo a seguir para la key dada (binary search).
    /// Retorna el índice `j` tal que `children[j]` contiene el rango de `key`.
    ///
    /// Busca el primer separador estrictamente mayor que `key` usando binary search.
    /// Las keys están ordenadas por invariante del B+ Tree → O(log n) comparaciones.
    pub fn find_child_idx(&self, key: &[u8]) -> usize {
        let n = self.num_keys();
        let mut lo = 0usize;
        let mut hi = n;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.key_at(mid) <= key {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }

    /// Inserta un par (sep_key, right_child_pid) en la posición `pos`.
    /// Desplaza los existentes hacia la derecha. Incrementa num_keys.
    ///
    /// Precondición: num_keys < ORDER_INTERNAL
    pub fn insert_at(&mut self, pos: usize, sep_key: &[u8], right_pid: u64) {
        let n = self.num_keys();
        debug_assert!(n < ORDER_INTERNAL, "nodo interno lleno antes de insert");

        // Desplazar keys y key_lens
        for i in (pos..n).rev() {
            self.keys[i + 1] = self.keys[i];
            self.key_lens[i + 1] = self.key_lens[i];
        }
        // Desplazar children (children[pos+1..=n+1])
        for i in (pos..=n).rev() {
            let pid = self.child_at(i);
            self.set_child_at(i + 1, pid);
        }

        self.set_key_at(pos, sep_key);
        self.set_child_at(pos + 1, right_pid);
        self.set_num_keys(n + 1);
    }

    /// Elimina la key en posición `key_pos` y el child en `child_pos`.
    /// Usado al mergear: se elimina el separador y uno de los hijos mergeados.
    pub fn remove_at(&mut self, key_pos: usize, child_pos: usize) {
        let n = self.num_keys();
        debug_assert!(n > 0);
        debug_assert!(key_pos < n);
        debug_assert!(child_pos <= n);

        for i in key_pos..n - 1 {
            self.keys[i] = self.keys[i + 1];
            self.key_lens[i] = self.key_lens[i + 1];
        }
        self.key_lens[n - 1] = 0;
        self.keys[n - 1].fill(0);

        for i in child_pos..n {
            let pid = self.child_at(i + 1);
            self.set_child_at(i, pid);
        }
        self.set_child_at(n, 0);
        self.set_num_keys(n - 1);
    }
}

// ── Nodo Hoja ─────────────────────────────────────────────────────────────────

/// Representación binaria de un nodo hoja en el body de una página.
///
/// Layout (16291 bytes):
/// ```text
/// Offset    Tamaño  Campo
///      0         1  is_leaf  (siempre 1)
///      1         1  _pad0
///      2         2  num_keys  (LE u16)
///      4         4  _pad1
///      8         8  next_leaf  (LE u64, NULL_PAGE si es la última hoja)
///     16       217  key_lens
///    233      2170  rids      (217 × [u8;10]: page_id(8 LE) + slot_id(2 LE))
///   2403     13888  keys      (217 × [u8;64], zero-padded)
/// Total: 16291
/// ```
#[repr(C)]
#[derive(Clone, Copy)]
pub struct LeafNodePage {
    pub is_leaf: u8,
    pub _pad0: u8,
    pub num_keys: [u8; 2],
    pub _pad1: [u8; 4],
    pub next_leaf: [u8; 8],
    pub key_lens: [u8; ORDER_LEAF],
    pub rids: [[u8; 10]; ORDER_LEAF],
    pub keys: [[u8; MAX_KEY_LEN]; ORDER_LEAF],
}

const _: () = assert!(
    size_of::<LeafNodePage>() == 16 + ORDER_LEAF + ORDER_LEAF * 10 + ORDER_LEAF * MAX_KEY_LEN,
    "LeafNodePage: tamaño incorrecto"
);
const _: () = assert!(
    size_of::<LeafNodePage>() <= PAGE_BODY_SIZE,
    "LeafNodePage no cabe en el body de una página"
);

// SAFETY: LeafNodePage es #[repr(C)] con todos los campos siendo arrays de u8.
// - No hay padding implícito (alignment = 1).
// - Cualquier secuencia de bits es válida (todos los campos son u8 o [u8;N]).
unsafe impl bytemuck::Zeroable for LeafNodePage {}
unsafe impl bytemuck::Pod for LeafNodePage {}

impl LeafNodePage {
    pub fn num_keys(&self) -> usize {
        u16::from_le_bytes(self.num_keys) as usize
    }

    pub fn set_num_keys(&mut self, n: usize) {
        self.num_keys = (n as u16).to_le_bytes();
    }

    pub fn next_leaf_val(&self) -> u64 {
        u64::from_le_bytes(self.next_leaf)
    }

    pub fn set_next_leaf(&mut self, pid: u64) {
        self.next_leaf = pid.to_le_bytes();
    }

    pub fn key_at(&self, i: usize) -> &[u8] {
        &self.keys[i][..self.key_lens[i] as usize]
    }

    pub fn set_key_at(&mut self, i: usize, k: &[u8]) {
        debug_assert!(k.len() <= MAX_KEY_LEN);
        self.key_lens[i] = k.len() as u8;
        self.keys[i][..k.len()].copy_from_slice(k);
        self.keys[i][k.len()..].fill(0);
    }

    pub fn rid_at(&self, i: usize) -> RecordId {
        decode_rid(self.rids[i])
    }

    pub fn set_rid_at(&mut self, i: usize, rid: RecordId) {
        self.rids[i] = encode_rid(rid);
    }

    /// Búsqueda binaria de `key` en el nodo hoja.
    /// Retorna `Ok(idx)` si existe, `Err(idx)` con posición de inserción si no.
    pub fn search(&self, key: &[u8]) -> Result<usize, usize> {
        let n = self.num_keys();
        let mut lo = 0usize;
        let mut hi = n;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            match self.key_at(mid).cmp(key) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return Ok(mid),
            }
        }
        Err(lo)
    }

    /// Inserta (key, rid) en la posición `pos`. Desplaza los existentes.
    /// Precondición: num_keys < ORDER_LEAF
    pub fn insert_at(&mut self, pos: usize, key: &[u8], rid: RecordId) {
        let n = self.num_keys();
        debug_assert!(n < ORDER_LEAF);

        for i in (pos..n).rev() {
            self.keys[i + 1] = self.keys[i];
            self.key_lens[i + 1] = self.key_lens[i];
            self.rids[i + 1] = self.rids[i];
        }
        self.set_key_at(pos, key);
        self.set_rid_at(pos, rid);
        self.set_num_keys(n + 1);
    }

    /// Elimina la entrada en posición `pos`. Desplaza los restantes.
    pub fn remove_at(&mut self, pos: usize) {
        let n = self.num_keys();
        debug_assert!(pos < n);

        for i in pos..n - 1 {
            self.keys[i] = self.keys[i + 1];
            self.key_lens[i] = self.key_lens[i + 1];
            self.rids[i] = self.rids[i + 1];
        }
        self.key_lens[n - 1] = 0;
        self.keys[n - 1].fill(0);
        self.rids[n - 1] = [0u8; 10];
        self.set_num_keys(n - 1);
    }
}

// ── Helpers de serialización de RecordId ────────────────────────────────────

/// Serializa RecordId a 10 bytes: page_id (8 LE) + slot_id (2 LE).
#[inline]
pub fn encode_rid(rid: RecordId) -> [u8; 10] {
    let mut buf = [0u8; 10];
    buf[..8].copy_from_slice(&rid.page_id.to_le_bytes());
    buf[8..10].copy_from_slice(&rid.slot_id.to_le_bytes());
    buf
}

/// Deserializa RecordId desde 10 bytes.
///
/// Usa indexación directa de arrays para evitar `try_into().unwrap()` en código
/// de producción — el compilador verifica los tamaños en tiempo de compilación.
#[inline]
pub fn decode_rid(buf: [u8; 10]) -> RecordId {
    RecordId {
        page_id: u64::from_le_bytes([
            buf[0], buf[1], buf[2], buf[3], buf[4], buf[5], buf[6], buf[7],
        ]),
        slot_id: u16::from_le_bytes([buf[8], buf[9]]),
    }
}

// ── Casts zero-copy ──────────────────────────────────────────────────────────

/// Obtiene una referencia inmutable al nodo interno desde el body de una página.
///
/// # SAFETY via bytemuck
/// `InternalNodePage` es `Pod` (todos bytes válidos para cualquier bit pattern).
/// El body tiene `PAGE_BODY_SIZE >= size_of::<InternalNodePage>()` bytes.
/// Se verifica con `assert_eq!(page.body()[0], 0)` antes de llamar en producción.
pub fn cast_internal(page: &nexusdb_storage::Page) -> &InternalNodePage {
    bytemuck::from_bytes(&page.body()[..size_of::<InternalNodePage>()])
}

/// Obtiene una referencia mutable al nodo interno desde el body de una página.
pub fn cast_internal_mut(page: &mut nexusdb_storage::Page) -> &mut InternalNodePage {
    bytemuck::from_bytes_mut(&mut page.body_mut()[..size_of::<InternalNodePage>()])
}

/// Obtiene una referencia inmutable al nodo hoja desde el body de una página.
pub fn cast_leaf(page: &nexusdb_storage::Page) -> &LeafNodePage {
    bytemuck::from_bytes(&page.body()[..size_of::<LeafNodePage>()])
}

/// Obtiene una referencia mutable al nodo hoja desde el body de una página.
pub fn cast_leaf_mut(page: &mut nexusdb_storage::Page) -> &mut LeafNodePage {
    bytemuck::from_bytes_mut(&mut page.body_mut()[..size_of::<LeafNodePage>()])
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use bytemuck::Zeroable;

    #[test]
    fn test_internal_node_size_fits_page() {
        assert!(size_of::<InternalNodePage>() <= PAGE_BODY_SIZE);
        // Verificar el valor exacto calculado en el spec
        assert_eq!(
            size_of::<InternalNodePage>(),
            8 + ORDER_INTERNAL + (ORDER_INTERNAL + 1) * 8 + ORDER_INTERNAL * MAX_KEY_LEN
        );
    }

    #[test]
    fn test_leaf_node_size_fits_page() {
        assert!(size_of::<LeafNodePage>() <= PAGE_BODY_SIZE);
        assert_eq!(
            size_of::<LeafNodePage>(),
            16 + ORDER_LEAF + ORDER_LEAF * 10 + ORDER_LEAF * MAX_KEY_LEN
        );
    }

    #[test]
    fn test_rid_encode_decode_roundtrip() {
        let rid = RecordId {
            page_id: 0xDEADBEEF_CAFE1234,
            slot_id: 0xABCD,
        };
        let encoded = encode_rid(rid);
        let decoded = decode_rid(encoded);
        assert_eq!(decoded.page_id, rid.page_id);
        assert_eq!(decoded.slot_id, rid.slot_id);
    }

    #[test]
    fn test_internal_node_key_ops() {
        let mut node = InternalNodePage::zeroed();
        node.is_leaf = 0;
        node.set_num_keys(0);

        // Insertar 3 hijos y 2 separadores manualmente
        node.set_child_at(0, 100);
        node.insert_at(0, b"key_b", 200);
        node.insert_at(1, b"key_d", 300);
        // Ahora: children=[100, 200, 300], keys=["key_b", "key_d"]
        assert_eq!(node.num_keys(), 2);
        assert_eq!(node.key_at(0), b"key_b");
        assert_eq!(node.key_at(1), b"key_d");
        assert_eq!(node.child_at(0), 100);
        assert_eq!(node.child_at(1), 200);
        assert_eq!(node.child_at(2), 300);
    }

    #[test]
    fn test_internal_find_child_idx() {
        let mut node = InternalNodePage::zeroed();
        node.set_num_keys(0);
        node.set_child_at(0, 10);
        node.insert_at(0, b"key_b", 20);
        node.insert_at(1, b"key_d", 30);
        // keys=["key_b","key_d"], children=[10,20,30]

        // key < "key_b" → idx=0
        assert_eq!(node.find_child_idx(b"aaa"), 0);
        // key == "key_b" → idx=1 (first separator > "key_b" is "key_d" at i=1)
        assert_eq!(node.find_child_idx(b"key_b"), 1);
        // key between "key_b" and "key_d"
        assert_eq!(node.find_child_idx(b"key_c"), 1);
        // key == "key_d" → idx=2
        assert_eq!(node.find_child_idx(b"key_d"), 2);
        // key > "key_d" → idx=2 (num_keys=2)
        assert_eq!(node.find_child_idx(b"zzz"), 2);
    }

    #[test]
    fn test_leaf_node_insert_remove() {
        let mut node = LeafNodePage::zeroed();
        node.is_leaf = 1;
        node.set_next_leaf(NULL_PAGE);

        let rid1 = RecordId {
            page_id: 1,
            slot_id: 0,
        };
        let rid2 = RecordId {
            page_id: 2,
            slot_id: 1,
        };

        assert_eq!(node.search(b"aaa"), Err(0));
        node.insert_at(0, b"bbb", rid1);
        assert_eq!(node.num_keys(), 1);
        assert_eq!(node.key_at(0), b"bbb");
        assert_eq!(node.rid_at(0).page_id, 1);

        // Insertar antes
        node.insert_at(0, b"aaa", rid2);
        assert_eq!(node.num_keys(), 2);
        assert_eq!(node.key_at(0), b"aaa");
        assert_eq!(node.key_at(1), b"bbb");

        // Eliminar el primero
        node.remove_at(0);
        assert_eq!(node.num_keys(), 1);
        assert_eq!(node.key_at(0), b"bbb");
    }

    #[test]
    fn test_leaf_search() {
        let mut node = LeafNodePage::zeroed();
        node.is_leaf = 1;
        let rid = RecordId {
            page_id: 1,
            slot_id: 0,
        };
        node.insert_at(0, b"ccc", rid);
        node.insert_at(1, b"eee", rid);
        node.insert_at(2, b"ggg", rid);

        assert_eq!(node.search(b"aaa"), Err(0)); // antes de ccc
        assert_eq!(node.search(b"ccc"), Ok(0));
        assert_eq!(node.search(b"ddd"), Err(1)); // entre ccc y eee
        assert_eq!(node.search(b"eee"), Ok(1));
        assert_eq!(node.search(b"ggg"), Ok(2));
        assert_eq!(node.search(b"zzz"), Err(3)); // después de ggg
    }
}
