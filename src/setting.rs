extern crate serde;
use serde::{Deserialize, Serialize};
use serde_either::StringOrStruct;
use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;

#[derive(Debug, Serialize, Deserialize)]
pub struct Key {
    pub files: Vec<String>,
    pub prefix: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Setting {
    paths: Vec<String>,
    key: StringOrStruct<Key>,
    fallback_keys: Vec<String>,
}
impl Setting {
    pub fn new_from_file(path: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let mut file = File::open(path)?;
        let mut contents = String::new();
        file.read_to_string(&mut contents)?;
        let conf: Self = toml::from_str(&contents)?;
        Ok(conf)
    }
    pub fn init_to_file(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
        let setting = Setting {
            paths: vec!["foo.txt".to_string()],
            key: StringOrStruct::Struct(Key {
                files: vec!["bar.txt".to_string()],
                prefix: None,
            }),
            fallback_keys: Default::default(),
        };
        let mut file = File::create(path)?;
        let toml = toml::to_string(&setting).unwrap();
        write!(file, "{}", toml)?;
        file.flush()?;
        Ok(())
    }
}
