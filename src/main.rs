use std::error::Error;

use envconfig::Envconfig;
use i3blocks_volume_pw::{Control, Config};

fn main() -> Result<(), Box<dyn Error>> {
    let config = Config::init_from_env()?;
    Control::new(config).run()
}
