/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::ffi::CString;
use std::fmt;
use std::io;
use std::path::PathBuf;
use std::str;

use thiserror::Error;

/// The error type for parsing config files.
#[derive(Error, Debug)]
pub enum Error {
    /// Unable to convert to a type.
    #[error("{0}")]
    Convert(String),

    /// Unable to parse a file due to syntax.
    #[error("{0:?}:\n{1}")]
    Parse(PathBuf, String),

    /// Unable to read a file due to IO errors.
    #[error("{0:?}: {1}")]
    Io(PathBuf, #[source] io::Error),

    /// Config file contains invalid UTF-8.
    #[error("{0:?}: {1}")]
    Utf8(PathBuf, #[source] str::Utf8Error),

    #[error("{0:?}: {1}")]
    Utf8Path(CString, #[source] str::Utf8Error),
}

#[derive(Error, Debug)]
pub struct Errors(pub Vec<Error>);

impl fmt::Display for Errors {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        for error in self.0.iter() {
            write!(f, "{}\n", error)?;
        }
        Ok(())
    }
}