mod env;
mod setting;
use bpaf::*;
use std::path::PathBuf;

#[derive(Debug, Clone, Bpaf)]
#[allow(dead_code)]
#[bpaf(options)]
struct Opts {
    #[bpaf(external)]
    action: Action,
}

#[derive(Debug, Clone, Bpaf)]
enum Action {
    #[bpaf(command)]
    Store { config: PathBuf },

    #[bpaf(command)]
    Restore { config: PathBuf },

    #[bpaf(command)]
    Init { config: PathBuf },
}

fn main() {
    let ops = opts().run();
    match ops.action {
        Action::Init { config } => {
            setting::Setting::init_to_file(&config).unwrap();
        }
        Action::Store { config } => {
            let environment = env::Env::new().unwrap();
            let setting = setting::Setting::new_from_file(&config).unwrap();
            println!("{:#?}", config);
            println!("{:#?}", environment);
            println!("{:#?}", setting);
        }
        Action::Restore { config } => {
            let environment = env::Env::new().unwrap();
            let setting = setting::Setting::new_from_file(&config).unwrap();
            println!("{:#?}", config);
            println!("{:#?}", environment);
            println!("{:#?}", setting);
        }
    }
}
