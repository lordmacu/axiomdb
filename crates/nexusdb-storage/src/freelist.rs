use nexusdb_core::error::DbError;

use crate::page::{HEADER_SIZE, PAGE_SIZE};

// ── Constantes ────────────────────────────────────────────────────────────────

/// Bytes disponibles en el body de la página de bitmap.
const BITMAP_BODY_BYTES: usize = PAGE_SIZE - HEADER_SIZE;
/// Máximo de páginas que cubre un solo bitmap (130,560).
pub const BITMAP_CAPACITY: u64 = (BITMAP_BODY_BYTES * 8) as u64;

// ── FreeList ──────────────────────────────────────────────────────────────────

/// Bitmap de páginas libres almacenado en memoria.
///
/// Convenio: bit = 1 → FREE, bit = 0 → USED.
/// Los bits se organizan en words de 64 bits, LSB-first:
///   word[0] cubre páginas 0-63, word[1] cubre 64-127, etc.
///
/// `alloc` usa `u64::trailing_zeros()` sobre el word invertido para encontrar
/// el primer bit libre en O(1) por word, O(n/64) en total.
#[derive(Debug)]
pub struct FreeList {
    words: Vec<u64>,
    total_pages: u64,
}

impl FreeList {
    /// Crea un nuevo bitmap para `total_pages` páginas.
    ///
    /// Las páginas en `reserved` quedan marcadas como USED.
    /// Todas las demás (dentro del rango) quedan marcadas como FREE.
    pub fn new(total_pages: u64, reserved: &[u64]) -> Self {
        assert!(
            total_pages <= BITMAP_CAPACITY,
            "total_pages {total_pages} supera BITMAP_CAPACITY {BITMAP_CAPACITY}"
        );

        let n_words = Self::words_needed(total_pages);
        // Iniciar todo a FREE (0xFF...).
        let mut words = vec![u64::MAX; n_words];

        // Marcar bits más allá de total_pages como USED (no existen).
        Self::mask_tail(&mut words, total_pages);

        // Marcar reservadas como USED.
        let mut fl = FreeList { words, total_pages };
        for &page_id in reserved {
            fl.mark_used(page_id);
        }
        fl
    }

    /// Deserializa un FreeList desde el body de la página de bitmap.
    pub fn from_bytes(bytes: &[u8], total_pages: u64) -> Self {
        assert!(bytes.len() >= BITMAP_BODY_BYTES);
        assert!(total_pages <= BITMAP_CAPACITY);

        let n_words = Self::words_needed(total_pages);
        let mut words = vec![0u64; n_words];
        for (i, w) in words.iter_mut().enumerate() {
            let off = i * 8;
            // El slice tiene exactamente 8 bytes: `off = i * 8` y
            // `bytes.len() >= BITMAP_BODY_BYTES` (assert arriba), con
            // `i < n_words` y `n_words * 8 <= BITMAP_BODY_BYTES`.
            *w = u64::from_le_bytes(
                bytes[off..off + 8]
                    .try_into()
                    .expect("slice de 8 bytes garantizado por invariante de BITMAP_BODY_BYTES"),
            );
        }
        // Garantizar que bits sobrantes están a cero (USED).
        Self::mask_tail(&mut words, total_pages);
        FreeList { words, total_pages }
    }

    /// Serializa el bitmap al buffer `buf` (debe ser ≥ BITMAP_BODY_BYTES).
    pub fn to_bytes(&self, buf: &mut [u8]) {
        assert!(buf.len() >= BITMAP_BODY_BYTES);
        // Limpiar primero (por si había datos viejos más allá del bitmap activo).
        buf[..BITMAP_BODY_BYTES].fill(0);
        for (i, &w) in self.words.iter().enumerate() {
            let off = i * 8;
            buf[off..off + 8].copy_from_slice(&w.to_le_bytes());
        }
    }

    /// Busca y reserva la primera página libre.
    ///
    /// Retorna `None` si el bitmap está lleno (tiempo de crecer).
    /// Complejidad: O(n/64) donde n = total_pages.
    pub fn alloc(&mut self) -> Option<u64> {
        for (i, word) in self.words.iter_mut().enumerate() {
            if *word == 0 {
                continue;
            }
            // trailing_zeros sobre word da el índice del bit libre más bajo.
            let bit = word.trailing_zeros() as u64;
            let page_id = i as u64 * 64 + bit;
            if page_id < self.total_pages {
                // Marcar como USED.
                *word &= !(1u64 << bit);
                return Some(page_id);
            }
        }
        None
    }

    /// Marca `page_id` como libre.
    ///
    /// Retorna error si `page_id` está fuera de rango o ya era libre (double-free).
    pub fn free(&mut self, page_id: u64) -> Result<(), DbError> {
        if page_id >= self.total_pages {
            return Err(DbError::PageNotFound { page_id });
        }
        let (word_idx, bit) = Self::bit_pos(page_id);
        let mask = 1u64 << bit;
        if self.words[word_idx] & mask != 0 {
            return Err(DbError::Other(format!(
                "double-free detectado en página {page_id}"
            )));
        }
        self.words[word_idx] |= mask;
        Ok(())
    }

    /// Marca `page_id` como USED sin verificar si ya lo estaba.
    pub fn mark_used(&mut self, page_id: u64) {
        if page_id >= self.total_pages {
            return;
        }
        let (word_idx, bit) = Self::bit_pos(page_id);
        self.words[word_idx] &= !(1u64 << bit);
    }

    /// Extiende el bitmap para cubrir `new_total` páginas.
    ///
    /// Las páginas nuevas (old_total..new_total) quedan marcadas como FREE.
    pub fn grow(&mut self, new_total: u64) {
        assert!(new_total > self.total_pages);
        assert!(
            new_total <= BITMAP_CAPACITY,
            "new_total {new_total} supera BITMAP_CAPACITY"
        );

        let old_total = self.total_pages;
        let new_n_words = Self::words_needed(new_total);

        // Extender el vector con words llenos de FREE.
        self.words.resize(new_n_words, u64::MAX);
        self.total_pages = new_total;

        // Asegurar que los bits en el último word viejo que antes estaban
        // marcados como "fuera de rango" (USED) ahora se marcan FREE.
        let old_n_words = Self::words_needed(old_total);
        if old_n_words > 0 {
            let last_old_idx = old_n_words - 1;
            let bits_in_last = old_total % 64;
            if bits_in_last != 0 {
                // El word tenía bits superiores forzados a USED; ahora son FREE.
                let free_mask = u64::MAX << bits_in_last;
                self.words[last_old_idx] |= free_mask;
            }
        }

        // Volver a enmascarar bits más allá de new_total.
        Self::mask_tail(&mut self.words, new_total);
    }

    /// Número total de páginas que cubre este bitmap.
    pub fn total_pages(&self) -> u64 {
        self.total_pages
    }

    /// Número de páginas libres actualmente.
    pub fn free_count(&self) -> u64 {
        self.words.iter().map(|w| w.count_ones() as u64).sum()
    }

    // ── Helpers privados ──────────────────────────────────────────────────────

    #[inline]
    fn words_needed(n_pages: u64) -> usize {
        n_pages.div_ceil(64) as usize
    }

    #[inline]
    fn bit_pos(page_id: u64) -> (usize, u32) {
        ((page_id / 64) as usize, (page_id % 64) as u32)
    }

    /// Fuerza a USED todos los bits del último word que estén más allá de `total`.
    fn mask_tail(words: &mut [u64], total: u64) {
        let remainder = total % 64;
        if remainder != 0 && !words.is_empty() {
            let last = words.len() - 1;
            // Bits [remainder..63] deben ser 0 (USED / no existen).
            let valid_mask = (1u64 << remainder) - 1;
            words[last] &= valid_mask;
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_fl(total: u64) -> FreeList {
        // Reservar páginas 0 y 1 como en el storage real.
        FreeList::new(total, &[0, 1])
    }

    #[test]
    fn test_alloc_starts_from_2() {
        let mut fl = make_fl(64);
        assert_eq!(fl.alloc(), Some(2));
        assert_eq!(fl.alloc(), Some(3));
        assert_eq!(fl.alloc(), Some(4));
    }

    #[test]
    fn test_alloc_consecutive_all_pages() {
        let mut fl = make_fl(64);
        let mut ids: Vec<u64> = (0..62).map(|_| fl.alloc().unwrap()).collect();
        ids.sort();
        assert_eq!(ids, (2u64..64).collect::<Vec<_>>());
        // Ahora está lleno.
        assert_eq!(fl.alloc(), None);
    }

    #[test]
    fn test_free_and_realloc() {
        let mut fl = make_fl(64);
        let id1 = fl.alloc().unwrap(); // 2
        let id2 = fl.alloc().unwrap(); // 3
        fl.free(id1).unwrap();
        // El siguiente alloc debe reutilizar id1 (es el menor libre).
        assert_eq!(fl.alloc(), Some(id1));
        assert_eq!(fl.alloc(), Some(id2 + 1));
    }

    #[test]
    fn test_double_free_is_error() {
        let mut fl = make_fl(64);
        let id = fl.alloc().unwrap();
        fl.free(id).unwrap();
        assert!(fl.free(id).is_err());
    }

    #[test]
    fn test_free_out_of_range_is_error() {
        let mut fl = make_fl(64);
        assert!(fl.free(100).is_err());
    }

    #[test]
    fn test_reserved_pages_never_allocated() {
        let mut fl = make_fl(64);
        let allocated: Vec<u64> = (0..62).map(|_| fl.alloc().unwrap()).collect();
        assert!(!allocated.contains(&0));
        assert!(!allocated.contains(&1));
    }

    #[test]
    fn test_serialization_roundtrip() {
        let mut fl = make_fl(128);
        fl.alloc().unwrap(); // 2
        fl.alloc().unwrap(); // 3
        fl.free(2).unwrap();

        let mut buf = vec![0u8; BITMAP_BODY_BYTES];
        fl.to_bytes(&mut buf);

        let fl2 = FreeList::from_bytes(&buf, 128);
        assert_eq!(fl2.free_count(), fl.free_count());
        // Página 2 libre, 3 usada.
        let mut fl2 = fl2;
        assert_eq!(fl2.alloc(), Some(2)); // fue liberada
    }

    #[test]
    fn test_grow_marks_new_pages_free() {
        let mut fl = make_fl(64);
        // Agotar todas las páginas.
        while fl.alloc().is_some() {}
        assert_eq!(fl.alloc(), None);

        fl.grow(128);
        // Después de grow, páginas 64..128 son libres.
        let id = fl.alloc().unwrap();
        assert!(id >= 64 && id < 128);
    }

    #[test]
    fn test_grow_preserves_used_pages() {
        let mut fl = make_fl(64);
        let ids: Vec<u64> = (0..5).map(|_| fl.alloc().unwrap()).collect();
        fl.grow(128);

        // Las páginas ya allocadas siguen USED (no se retornan por alloc).
        let new_allocs: Vec<u64> = (0..10).map(|_| fl.alloc().unwrap()).collect();
        for id in &ids {
            assert!(!new_allocs.contains(id), "página {id} ya usada reaparecio");
        }
    }

    #[test]
    fn test_free_count() {
        let mut fl = make_fl(64);
        assert_eq!(fl.free_count(), 62); // 64 - 2 reservadas
        fl.alloc().unwrap();
        assert_eq!(fl.free_count(), 61);
    }

    #[test]
    fn test_cross_word_boundary() {
        // Verificar que alloc funciona correctamente cruzando el boundary de 64 páginas.
        let mut fl = make_fl(128);
        // Agotar el primer word completo (páginas 2..64).
        for _ in 0..62 {
            fl.alloc().unwrap();
        }
        // El siguiente alloc debe saltar al segundo word.
        let id = fl.alloc().unwrap();
        assert_eq!(id, 64);
    }
}
