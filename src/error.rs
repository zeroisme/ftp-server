use std::io;
use std::str::Utf8Error;
use std::string::FromUtf8Error;

pub enum Error {
    FromUtf8(FromUtf8Error),
    Io(io::Error),
    Msg(String),
    Utf8(Utf8Error),
}