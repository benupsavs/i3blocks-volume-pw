use std::{error::Error, io::{self, BufReader, BufRead}, thread};

use envconfig::Envconfig;
use i3blocks_volume_pw::{Control, Config, parse_click};

fn main() -> Result<(), Box<dyn Error>> {
    let config = Config::init_from_env()?;
    let mut control = Control::new(config);
    control.subscribe()?;

    let tx_for_click_listener = control.tx();

    let click_listener_thread = thread::Builder::new()
        .name("click_listener".to_string())
        .stack_size(16 * 1024)
        .spawn(move || {
            let mut stdin_reader = BufReader::new(io::stdin());
            let mut line_buffer = String::new();

            // Send initial event to trigger first refresh
            if tx_for_click_listener.send(0).is_err() {
                eprintln!("Click listener: Failed to send initial event. Receiver likely gone.");
                return;
            }

            loop {
                line_buffer.clear();
                match stdin_reader.read_line(&mut line_buffer) {
                    Ok(0) => { // EOF
                        break;
                    }
                    Ok(_) => { // Bytes read > 0
                        // Attempt to parse as a click event
                        if let Ok(click) = parse_click(line_buffer.trim_end()) {
                            if tx_for_click_listener.send(click.button).is_err() {
                                eprintln!("Click listener: Failed to send click event. Receiver likely gone.");
                                break;
                            }
                        } else {
                            // Line was not a valid click JSON.
                            // If any non-empty line (that's not a click) should trigger a refresh:
                            if !line_buffer.trim().is_empty() {
                                if tx_for_click_listener.send(0).is_err() { // Send '0' for generic event
                                    eprintln!("Click listener: Failed to send generic event. Receiver likely gone.");
                                    break;
                                }
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("Click listener: Error reading from stdin: {}. Exiting.", e);
                        break;
                    }
                }
            }
        })?;

    control.refresh_loop();

    // Wait for the click listener thread to finish.
    // It should exit when stdin closes or when tx.send fails (if refresh_loop exits first).
    if let Err(e) = click_listener_thread.join() {
        eprintln!("Error joining click listener thread: {:?}", e);
    }

    Ok(())
}
