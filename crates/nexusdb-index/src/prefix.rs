//! Prefix compression para nodos internos del B+ Tree.
//!
//! Los nodos internos suelen tener keys con prefijos comunes (e.g., todos empiezan
//! con `"usuario:"`). Extraer el prefijo una sola vez permite almacenar solo los
//! sufijos únicos, reduciendo el uso de RAM y mejorando cache locality.
//!
//! Esta compresión opera **solo en memoria** — el layout en disco no cambia.

/// Nodo interno con compresión de prefijos.
///
/// # Ejemplo
/// ```
/// use nexusdb_index::prefix::CompressedNode;
///
/// let keys: Vec<Box<[u8]>> = vec![
///     b"usuario:00001".to_vec().into_boxed_slice(),
///     b"usuario:00002".to_vec().into_boxed_slice(),
///     b"usuario:00003".to_vec().into_boxed_slice(),
/// ];
/// let children = vec![10u64, 20, 30, 40];
/// let node = CompressedNode::from_keys(&keys, children);
/// // prefijo común de "usuario:00001/2/3" = "usuario:0000"
/// assert_eq!(node.common_prefix, b"usuario:0000");
/// assert_eq!(node.reconstruct_key(0), b"usuario:00001");
/// ```
pub struct CompressedNode {
    /// Prefijo común de todas las keys del nodo.
    pub common_prefix: Vec<u8>,
    /// Sufijos (la parte única de cada key, sin el prefijo).
    pub suffixes: Vec<Vec<u8>>,
    /// Punteros a páginas hijas (len = suffixes.len() + 1).
    pub children: Vec<u64>,
}

impl CompressedNode {
    /// Construye un `CompressedNode` desde keys y children.
    ///
    /// Si todas las keys comparten un prefijo, se extrae. Si no hay prefijo común
    /// (o hay 0 keys), `common_prefix` queda vacío.
    pub fn from_keys(keys: &[Box<[u8]>], children: Vec<u64>) -> Self {
        debug_assert_eq!(
            children.len(),
            keys.len() + 1,
            "children.len() debe ser keys.len() + 1"
        );

        let common_prefix = Self::find_common_prefix(keys);
        let plen = common_prefix.len();
        let suffixes = keys.iter().map(|k| k[plen..].to_vec()).collect();

        Self {
            common_prefix,
            suffixes,
            children,
        }
    }

    /// Reconstruye la key completa en posición `idx`.
    pub fn reconstruct_key(&self, idx: usize) -> Vec<u8> {
        let mut key = self.common_prefix.clone();
        key.extend_from_slice(&self.suffixes[idx]);
        key
    }

    /// Encuentra el child page_id para una `search_key` dada.
    ///
    /// Equivalente a `find_child_idx` pero operando sobre los sufijos comprimidos.
    pub fn find_child(&self, search_key: &[u8]) -> u64 {
        let n = self.suffixes.len();
        let plen = self.common_prefix.len();

        // Comparar con el prefijo primero
        if search_key.len() < plen || &search_key[..plen] != self.common_prefix.as_slice() {
            // Si search_key < common_prefix: ir al primer child
            // Si search_key > todos: ir al último child
            if search_key < self.common_prefix.as_slice() {
                return self.children[0];
            }
            return self.children[n];
        }

        let suffix = &search_key[plen..];
        let child_idx = (0..n)
            .find(|&i| self.suffixes[i].as_slice() > suffix)
            .unwrap_or(n);
        self.children[child_idx]
    }

    /// Longitud del prefijo común de una lista de keys.
    pub fn common_prefix_len(keys: &[Box<[u8]>]) -> usize {
        Self::find_common_prefix(keys).len()
    }

    /// Calcula el ahorro en bytes respecto a almacenar keys sin comprimir.
    pub fn bytes_saved(&self) -> usize {
        let plen = self.common_prefix.len();
        plen * self.suffixes.len()
    }

    fn find_common_prefix(keys: &[Box<[u8]>]) -> Vec<u8> {
        if keys.is_empty() {
            return Vec::new();
        }

        let first = &keys[0];
        let mut plen = first.len();

        for key in &keys[1..] {
            plen = first
                .iter()
                .zip(key.iter())
                .take_while(|(a, b)| a == b)
                .count();
            if plen == 0 {
                return Vec::new();
            }
        }
        first[..plen].to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bkey(s: &[u8]) -> Box<[u8]> {
        s.to_vec().into_boxed_slice()
    }

    #[test]
    fn test_prefix_extraction() {
        let keys = vec![
            bkey(b"usuario:00001"),
            bkey(b"usuario:00002"),
            bkey(b"usuario:00003"),
        ];
        let children = vec![1u64, 2, 3, 4];
        let node = CompressedNode::from_keys(&keys, children);
        // prefijo común = "usuario:0000" (los 3 difieren solo en el último dígito)
        assert_eq!(node.common_prefix, b"usuario:0000");
        assert_eq!(node.suffixes[0], b"1");
        assert_eq!(node.suffixes[1], b"2");
        assert_eq!(node.suffixes[2], b"3");
    }

    #[test]
    fn test_reconstruct_key() {
        let keys = vec![bkey(b"order:00100"), bkey(b"order:00200")];
        let children = vec![10u64, 20, 30];
        let node = CompressedNode::from_keys(&keys, children);
        assert_eq!(node.reconstruct_key(0), b"order:00100");
        assert_eq!(node.reconstruct_key(1), b"order:00200");
    }

    #[test]
    fn test_no_common_prefix() {
        let keys = vec![bkey(b"abc"), bkey(b"xyz")];
        let children = vec![1u64, 2, 3];
        let node = CompressedNode::from_keys(&keys, children);
        assert!(node.common_prefix.is_empty());
        assert_eq!(node.suffixes[0], b"abc");
        assert_eq!(node.suffixes[1], b"xyz");
    }

    #[test]
    fn test_find_child() {
        let keys = vec![bkey(b"item:0010"), bkey(b"item:0020"), bkey(b"item:0030")];
        let children = vec![100u64, 200, 300, 400];
        let node = CompressedNode::from_keys(&keys, children.clone());

        assert_eq!(node.find_child(b"item:0005"), 100); // antes del primero
        assert_eq!(node.find_child(b"item:0010"), 200); // igual al primero → right
        assert_eq!(node.find_child(b"item:0015"), 200); // entre primero y segundo
        assert_eq!(node.find_child(b"item:0020"), 300);
        assert_eq!(node.find_child(b"item:0030"), 400);
        assert_eq!(node.find_child(b"item:0099"), 400); // después del último
    }

    #[test]
    fn test_bytes_saved() {
        let keys = vec![
            bkey(b"prefix_long:aaa"),
            bkey(b"prefix_long:bbb"),
            bkey(b"prefix_long:ccc"),
        ];
        let children = vec![1u64, 2, 3, 4];
        let node = CompressedNode::from_keys(&keys, children);
        // prefijo = "prefix_long:" = 12 bytes × 3 keys = 36 bytes ahorrados
        assert_eq!(node.bytes_saved(), 12 * 3);
    }

    #[test]
    fn test_empty_keys() {
        let node = CompressedNode::from_keys(&[], vec![42]);
        assert!(node.common_prefix.is_empty());
        assert!(node.suffixes.is_empty());
    }
}
