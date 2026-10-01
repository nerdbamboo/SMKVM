//! The Win32 half: the calls that only Windows can answer.
//!
//! Everything with a decision in it is in the modules beside this one,
//! where it is tested on whatever machine happens to be building. What is
//! left here is the doing, and it is kept as close to a transcription of
//! the documentation as it can be -- because none of it can be run on the
//! machine it is written on, and code that cannot be run is code whose only
//! defence is being obvious.

/// A byte buffer aligned well enough for the structures the system
/// writes into it.
///
/// `GetTokenInformation` and `QueryServiceConfigW` both fill a caller's
/// buffer with a structure containing pointers and then expect it to be
/// read back as that structure. Done with a `Vec<u8>`, whose elements are
/// aligned to one byte, taking a reference to the structure inside is
/// undefined behaviour in Rust however well the hardware tolerates it:
/// the reference must be aligned whether or not the read would have
/// worked. Backing the bytes with `u64` gives eight-byte alignment, which
/// is what a pointer on this target wants, and costs a rounding-up of the
/// length.
pub(crate) struct Aligned(Vec<u64>);

impl Aligned {
    pub(crate) fn new(bytes: usize) -> Self {
        Aligned(vec![0u64; bytes.div_ceil(8).max(1)])
    }

    pub(crate) fn len(&self) -> usize {
        self.0.len() * 8
    }

    pub(crate) fn as_mut_ptr(&mut self) -> *mut u8 {
        self.0.as_mut_ptr() as *mut u8
    }

    pub(crate) fn as_ptr(&self) -> *const u8 {
        self.0.as_ptr() as *const u8
    }
}

pub mod clip;
pub mod link;
pub mod pipe;
pub mod probe;
pub mod scm;
pub mod secret;
pub mod serving;
pub mod store;
pub mod token;
pub mod worker;
