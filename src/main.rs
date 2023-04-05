use std::{error::Error, io::{self, BufReader, BufRead}, thread};

use envconfig::Envconfig;
use i3blocks_volume_pw::{Control, Config, parse_click};

fn main() -> Result<(), Box<dyn Error>> {
    let config = Config::init_from_env()?;
    let mut control = Control::new(config);
    control.subscribe();
    let tx = control.tx().ok_or::<Box<dyn std::error::Error>>("tx not initialized".into())?;

    let t = thread::Builder::new().name("click listener".to_string()).stack_size(16 * 1024).spawn(move || {
        let mut input = BufReader::new(io::stdin());
        let mut line = String::new();
        if let Err(e) = tx.send(0) {
            println!("{}", e);
            return;
        }
        loop {
            match input.read_line(&mut line) {
                Ok(s) => {
                    if let Ok(click) = parse_click(&line) {
                        if let Err(e) = tx.send(click.button) {
                            println!("Error sending event: {e}");
                        }
                    }
                    if s > 1 && tx.send(0).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
            line.clear();
        }
    })?;
    control.refresh_loop();
    _ = t.join();

    Ok(())
}
