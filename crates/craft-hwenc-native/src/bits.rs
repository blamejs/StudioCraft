//! Reading parameter sets: emulation prevention bytes removed (ITU-T H.264 7.4.1 and H.265
//! 7.4.2), then fixed-width and Exp-Golomb fields read most significant bit first (H.264 7.2 and
//! 9.1, H.265 7.2 and 9.2).

/// The RBSP of a NAL unit payload: every `00 00 03` loses its `03`.
pub fn unescape(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len());
    let mut zeros = 0usize;
    for &b in payload {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

/// A reader of the bits of an RBSP. Every read past the end is an error.
pub struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
}

/// The read ran past the end of the data, or an Exp-Golomb code was longer than 32 bits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Truncated;

impl<'a> Bits<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Bits { data, pos: 0 }
    }

    pub fn bit(&mut self) -> Result<u32, Truncated> {
        let byte = self.data.get(self.pos / 8).ok_or(Truncated)?;
        let b = (byte >> (7 - self.pos % 8)) & 1;
        self.pos += 1;
        Ok(u32::from(b))
    }

    /// `n` bits (at most 32) as an unsigned number.
    pub fn bits(&mut self, n: u32) -> Result<u32, Truncated> {
        let mut v = 0u32;
        for _ in 0..n.min(32) {
            v = (v << 1) | self.bit()?;
        }
        Ok(v)
    }

    pub fn skip(&mut self, n: usize) -> Result<(), Truncated> {
        let end = self.pos.checked_add(n).ok_or(Truncated)?;
        if end > self.data.len().saturating_mul(8) {
            return Err(Truncated);
        }
        self.pos = end;
        Ok(())
    }

    /// An unsigned Exp-Golomb code, `ue(v)`.
    pub fn ue(&mut self) -> Result<u32, Truncated> {
        let mut zeros = 0u32;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return Err(Truncated);
            }
        }
        let rest = self.bits(zeros)?;
        u32::try_from((1u64 << zeros) - 1 + u64::from(rest)).map_err(|_| Truncated)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emulation_prevention_bytes_are_removed() {
        assert_eq!(unescape(&[0, 0, 3, 1, 0, 0, 3, 0, 0, 3]), [0, 0, 1, 0, 0, 0, 0]);
        // a 03 after a single zero stays
        assert_eq!(unescape(&[0, 3, 0, 0, 2]), [0, 3, 0, 0, 2]);
        assert!(unescape(&[]).is_empty());
    }

    #[test]
    fn exp_golomb_codes_read_as_the_standard_numbers_them() {
        // 1 = 0, 010 = 1, 011 = 2, 00100 = 3, 00111 = 6, then 0001000 = 7
        let data = [0b1010_0110, 0b0100_0011, 0b1000_1000];
        let mut b = Bits::new(&data);
        assert_eq!([b.ue(), b.ue(), b.ue(), b.ue(), b.ue(), b.ue()], [Ok(0), Ok(1), Ok(2), Ok(3), Ok(6), Ok(7)]);
        assert_eq!(b.ue(), Err(Truncated));
    }

    #[test]
    fn reads_past_the_end_are_errors() {
        let mut b = Bits::new(&[0xff]);
        assert_eq!(b.bits(8), Ok(0xff));
        assert_eq!(b.bit(), Err(Truncated));
        assert_eq!(Bits::new(&[0; 5]).ue(), Err(Truncated), "a code longer than 32 bits");
        assert_eq!(Bits::new(&[0]).skip(9), Err(Truncated));
        assert_eq!(Bits::new(&[]).skip(usize::MAX), Err(Truncated));
    }
}
