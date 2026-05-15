/// Lock modes for the row-level lock manager.
///
/// 5-mode system inspired by InnoDB (`lock0types.h` LOCK_IS..LOCK_AUTO_INC).
/// Simpler than PostgreSQL's 8 modes — sufficient for row-level MVCC with
/// intention locking at the table level.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LockMode {
    /// Table-level: "I intend to read rows in this table."
    IntentionShared = 0,
    /// Table-level: "I intend to modify rows in this table."
    IntentionExclusive = 1,
    /// Row-level: "I'm reading this row." Multiple S locks are compatible.
    Shared = 2,
    /// Row-level: "I'm modifying this row." Blocks everything.
    Exclusive = 3,
    /// Table-level: brief lock during AUTO_INCREMENT id generation.
    AutoIncrement = 4,
}

impl LockMode {
    pub const COUNT: usize = 5;

    /// Returns a human-readable short name for logging.
    pub const fn short_name(self) -> &'static str {
        match self {
            Self::IntentionShared => "IS",
            Self::IntentionExclusive => "IX",
            Self::Shared => "S",
            Self::Exclusive => "X",
            Self::AutoIncrement => "AI",
        }
    }
}

/// Conflict matrix: `CONFLICT_MATRIX[held][requested]` is `true` when the two
/// modes are **incompatible** (the requester must wait).
///
/// Derived from InnoDB `lock_compatibility_matrix` in `lock0lock.cc` and the
/// spec at `specs/fase-40/spec-40.5-lock-manager.md` lines 43-49.
///
/// ```text
///           IS    IX    S     X     AI
///   IS      ✓     ✓     ✓     ✗     ✓
///   IX      ✓     ✓     ✗     ✗     ✓
///   S       ✓     ✗     ✓     ✗     ✗
///   X       ✗     ✗     ✗     ✗     ✗
///   AI      ✓     ✓     ✗     ✗     ✗
/// ```
pub const CONFLICT_MATRIX: [[bool; LockMode::COUNT]; LockMode::COUNT] = {
    const F: bool = false;
    const T: bool = true;
    //                IS     IX     S      X      AI
    [
        /* IS */ [F, F, F, T, F],
        /* IX */ [F, F, T, T, F],
        /* S  */ [F, T, F, T, T],
        /* X  */ [T, T, T, T, T],
        /* AI */ [F, F, T, T, T],
    ]
};

/// Returns `true` if `held` and `requested` modes are incompatible.
#[inline]
pub const fn conflicts(held: LockMode, requested: LockMode) -> bool {
    CONFLICT_MATRIX[held as usize][requested as usize]
}

/// Lock flags — packed into a `u16` bitfield.
///
/// Inspired by InnoDB `lock0types.h` flags (LOCK_WAIT, LOCK_GAP, etc.).
/// Manual newtype instead of `bitflags!` crate to avoid an extra workspace dep.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LockFlags(u16);

impl LockFlags {
    pub const NONE: Self = Self(0);
    /// Lock request is waiting (not yet granted).
    pub const WAITING: Self = Self(0x0001);
    /// Gap lock: locks the gap before a record (prevents phantom inserts).
    pub const GAP: Self = Self(0x0002);
    /// Record-only lock: no gap protection.
    pub const REC_NOT_GAP: Self = Self(0x0004);
    /// Insert intention gap lock: compatible with other insert intentions.
    pub const INSERT_INTENTION: Self = Self(0x0008);
    /// Table-level lock (vs row-level). Set for IS/IX/AI/table-X.
    pub const TABLE_LOCK: Self = Self(0x0010);
    /// Skip Condvar wait; return LockTimeout immediately on conflict.
    pub const NOWAIT: Self = Self(0x0020);

    #[inline]
    pub const fn empty() -> Self {
        Self(0)
    }

    #[inline]
    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0 && other.0 != 0
    }

    #[inline]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    #[inline]
    pub const fn difference(self, other: Self) -> Self {
        Self(self.0 & !other.0)
    }

    #[inline]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    #[inline]
    pub const fn bits(self) -> u16 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conflict_matrix_all_25_cells() {
        use LockMode::*;
        let modes = [
            IntentionShared,
            IntentionExclusive,
            Shared,
            Exclusive,
            AutoIncrement,
        ];

        // Expected: false = compatible, true = conflict
        let expected = [
            //         IS     IX     S      X      AI
            /* IS */
            [false, false, false, true, false],
            /* IX */ [false, false, true, true, false],
            /* S  */ [false, true, false, true, true],
            /* X  */ [true, true, true, true, true],
            /* AI */ [false, false, true, true, true],
        ];

        for (i, held) in modes.iter().enumerate() {
            for (j, requested) in modes.iter().enumerate() {
                assert_eq!(
                    conflicts(*held, *requested),
                    expected[i][j],
                    "{} vs {} should be {}",
                    held.short_name(),
                    requested.short_name(),
                    if expected[i][j] {
                        "conflict"
                    } else {
                        "compatible"
                    }
                );
            }
        }
    }

    #[test]
    fn x_conflicts_with_everything() {
        use LockMode::*;
        for mode in [
            IntentionShared,
            IntentionExclusive,
            Shared,
            Exclusive,
            AutoIncrement,
        ] {
            assert!(
                conflicts(Exclusive, mode),
                "X should conflict with {}",
                mode.short_name()
            );
        }
    }

    #[test]
    fn is_compatible_with_most() {
        use LockMode::*;
        assert!(!conflicts(IntentionShared, IntentionShared));
        assert!(!conflicts(IntentionShared, IntentionExclusive));
        assert!(!conflicts(IntentionShared, Shared));
        assert!(conflicts(IntentionShared, Exclusive));
        assert!(!conflicts(IntentionShared, AutoIncrement));
    }

    #[test]
    fn lock_flags_operations() {
        let f = LockFlags::GAP.union(LockFlags::WAITING);
        assert!(f.contains(LockFlags::GAP));
        assert!(f.contains(LockFlags::WAITING));
        assert!(!f.contains(LockFlags::TABLE_LOCK));

        let cleared = f.difference(LockFlags::WAITING);
        assert!(!cleared.contains(LockFlags::WAITING));
        assert!(cleared.contains(LockFlags::GAP));
    }
}
