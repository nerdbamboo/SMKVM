//! The Win32 half: the calls that only Windows can answer.
//!
//! Everything with a decision in it is in the modules beside this one,
//! where it is tested on whatever machine happens to be building. What is
//! left here is the doing, and it is kept as close to a transcription of
//! the documentation as it can be -- because none of it can be run on the
//! machine it is written on, and code that cannot be run is code whose only
//! defence is being obvious.

pub mod link;
pub mod pipe;
pub mod scm;
pub mod token;
pub mod worker;
