//! A lifted (quasi-cyclic) instance of the Ion protograph: encoder,
//! syndrome check and the lifted adjacency the decoder walks.

use super::design::{CORE_A, full_rows};
use super::tables::{BASE, CORE_ROWS, KB, PUNCT};

/// Largest check-node degree the decoder supports.
pub(crate) const MAX_DEG: usize = 32;

pub(crate) struct QcCode {
    /// Circulant size.
    pub z: usize,
    /// Base rows (checks) and columns (variables).
    pub mb: usize,
    pub nb: usize,
    /// Per base row: `(column, shift)`. Check `i` of row `r` touches bit
    /// `(i + shift) mod z` of column block `column`.
    pub rows: Vec<Vec<(usize, usize)>>,
    /// Lifted adjacency: the neighbours of check `i` of base row `r` are
    /// `adj[row_off[r] + i * deg[r]..][..deg[r]]`.
    pub adj: Vec<u32>,
    pub row_off: Vec<usize>,
    pub deg: Vec<usize>,
}

impl QcCode {
    /// Lifts the protograph in `tables.rs` with the shift table `shifts`
    /// (free entries only, aligned with `BASE`).
    pub fn new(shifts: &[&[u16]], z: usize) -> Self {
        let free: Vec<Vec<usize>> = BASE.iter().map(|r| r.iter().map(|&c| c as usize).collect()).collect();
        assert_eq!(shifts.len(), free.len());
        let rows: Vec<Vec<(usize, usize)>> = full_rows(&free)
            .into_iter()
            .zip(shifts)
            .map(|(row, sh)| {
                let mut k = 0;
                row.into_iter()
                    .map(|(c, fixed)| {
                        let s = fixed.unwrap_or_else(|| {
                            k += 1;
                            sh[k - 1]
                        });
                        (c, s as usize % z)
                    })
                    .collect::<Vec<_>>()
            })
            .collect();
        let mb = rows.len();
        let nb = KB + mb;
        let mut adj = Vec::new();
        let mut row_off = Vec::with_capacity(mb);
        let mut deg = Vec::with_capacity(mb);
        for row in &rows {
            assert!(row.len() <= MAX_DEG);
            row_off.push(adj.len());
            deg.push(row.len());
            for i in 0..z {
                for &(c, s) in row {
                    adj.push((c * z + (i + s) % z) as u32);
                }
            }
        }
        Self {
            z,
            mb,
            nb,
            rows,
            adj,
            row_off,
            deg,
        }
    }

    /// Information bits.
    pub fn k(&self) -> usize {
        KB * self.z
    }

    /// Length of the full mother codeword, punctured columns included.
    pub fn n_full(&self) -> usize {
        self.nb * self.z
    }

    /// First transmitted position: the punctured columns come first.
    pub fn tx_start(&self) -> usize {
        PUNCT * self.z
    }

    /// Most bits that can be sent (the lowest-rate mother code).
    pub fn n_tx_max(&self) -> usize {
        self.n_full() - self.tx_start()
    }

    /// Number of checks of base row `r` that are active when the first
    /// `n_tx` bits of the transmission order have been sent. Core checks
    /// are always active; an extension check is active once its degree-1
    /// parity bit is sent (otherwise it carries no information).
    pub fn active(&self, r: usize, n_tx: usize) -> usize {
        if r < CORE_ROWS {
            return self.z;
        }
        let start = (KB + r) * self.z;
        (self.tx_start() + n_tx).saturating_sub(start).min(self.z)
    }

    /// `acc[i] ^= x[(i + s) mod z]`.
    fn xor_shifted(acc: &mut [u8], x: &[u8], s: usize) {
        let z = acc.len();
        let (a, b) = acc.split_at_mut(z - s);
        for (o, &v) in a.iter_mut().zip(&x[s..]) {
            *o ^= v;
        }
        for (o, &v) in b.iter_mut().zip(&x[..s]) {
            *o ^= v;
        }
    }

    /// Systematic encoding into the full mother codeword (`n_full` bits):
    /// the core parity by the dual-diagonal back-substitution, then each
    /// extension parity block as the XOR of its row. Linear time.
    pub fn encode(&self, info: &[u8]) -> Vec<u8> {
        let z = self.z;
        assert_eq!(info.len(), self.k(), "info must be K bits");
        let mut x = vec![0u8; self.n_full()];
        for (o, &b) in x.iter_mut().zip(info) {
            *o = b & 1;
        }
        // lambda_r: info part of core row r.
        let mut lam = vec![vec![0u8; z]; CORE_ROWS];
        for (r, l) in lam.iter_mut().enumerate() {
            for &(c, s) in &self.rows[r] {
                if c < KB {
                    Self::xor_shifted(l, &x[c * z..(c + 1) * z], s);
                }
            }
        }
        // Sum of the four core rows: P^a + I + P^a = I on p0, the other
        // parity blocks cancel. So p0 = sum of lambdas.
        let mut p = vec![vec![0u8; z]; CORE_ROWS];
        for l in &lam {
            for (o, &v) in p[0].iter_mut().zip(l) {
                *o ^= v;
            }
        }
        // Row 0: P^a p0 + p1 = lambda0.
        let a = CORE_A as usize % z;
        let mut t = lam[0].clone();
        Self::xor_shifted(&mut t, &p[0], a);
        p[1] = t;
        // Row 1: p0 + p1 + p2 = lambda1.
        p[2] = (0..z).map(|i| lam[1][i] ^ p[0][i] ^ p[1][i]).collect();
        // Row 2: p2 + p3 = lambda2.
        p[3] = (0..z).map(|i| lam[2][i] ^ p[2][i]).collect();
        for (j, pj) in p.iter().enumerate() {
            x[(KB + j) * z..(KB + j + 1) * z].copy_from_slice(pj);
        }
        // Extension rows (only reference columns before their own parity).
        for (r, row) in self.rows.iter().enumerate().take(self.mb).skip(CORE_ROWS) {
            let own = KB + r;
            let mut acc = vec![0u8; z];
            for &(c, s) in row {
                if c != own {
                    debug_assert!(c < own);
                    Self::xor_shifted(&mut acc, &x[c * z..(c + 1) * z], s);
                }
            }
            x[own * z..(own + 1) * z].copy_from_slice(&acc);
        }
        x
    }

    /// True when the hard bits `x` (full mother frame) satisfy every check
    /// of every base row in `rows`, limited to the active checks for
    /// `n_tx` transmitted bits.
    pub fn checks_ok(&self, n_tx: usize, mut bit: impl FnMut(usize) -> u8) -> bool {
        for r in 0..self.mb {
            let d = self.deg[r];
            for i in 0..self.active(r, n_tx) {
                let nb = &self.adj[self.row_off[r] + i * d..][..d];
                let mut s = 0u8;
                for &v in nb {
                    s ^= bit(v as usize);
                }
                if s != 0 {
                    return false;
                }
            }
        }
        true
    }
}
