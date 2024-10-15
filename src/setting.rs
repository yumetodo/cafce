extern crate serde;
use serde::{Deserialize, Serialize};
use serde_either::StringOrStruct;
use std::fs::File;
use std::io::Write;

#[derive(Debug, Serialize, Deserialize)]
pub struct Key {
    files: Vec<String>,
    prefix: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Setting {
    paths: Vec<String>,
    key: StringOrStruct<Key>,
    fallback_keys: Vec<String>,
}
