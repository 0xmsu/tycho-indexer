pub mod cli;
pub mod extractor;
pub mod pb;
pub mod services;
pub mod substreams;
mod testing;

#[cfg(test)]
#[macro_use]
extern crate pretty_assertions;
