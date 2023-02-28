use std::{error::Error, io::{self, BufReader, BufRead}, thread};

use envconfig::Envconfig;
use i3blocks_volume_pw::{Control, Config};

fn main() -> Result<(), Box<dyn Error>> {
    let config = Config::init_from_env().expect("unable to read config from environment");
    let mut control = Control::new(config);
    control.subscribe();
    let tx = control.tx().unwrap();

    let t = thread::spawn(move || {
        let mut input = BufReader::new(io::stdin());
        let mut line = String::new();
        if let Err(e) = tx.send(0) {
            println!("{}", e);
            return;
        }
        loop {
            match input.read_line(&mut line) {
                Ok(l) => {
                    if let Ok(button) = line.trim_end().parse::<u8>() {
                        if let Err(e) = tx.send(button) {
                            println!("Error sending event: {e}");
                        }
                    }
                    if l > 1 {
                        if let Err(_) = tx.send(0) {
                            break;
                        }
                    }
                }
                Err(_) => break,
            }
            line.clear();
        }
    });
    control.refresh_loop();
    _ = t.join();

    Ok(())
}
