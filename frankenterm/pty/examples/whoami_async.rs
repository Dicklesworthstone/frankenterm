use anyhow::anyhow;
use asupersync::runtime::RuntimeBuilder;
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use std::io::BufRead;

// This example shows how to use `portable_pty` in an asynchronous application
// backed by the asupersync runtime.

fn main() -> anyhow::Result<()> {
    let runtime = RuntimeBuilder::current_thread().build()?;
    runtime.block_on(async {
        let pty_system = native_pty_system();

        let pair = pty_system.openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })?;

        let cmd = CommandBuilder::new("whoami");

        // Move the slave to a blocking worker thread to spawn the command.
        // This implicitly drops the slave and closes handles, which avoids
        // deadlocks when waiting for child completion.
        let slave = pair.slave;
        let mut child =
            asupersync::runtime::spawn_blocking(move || slave.spawn_command(cmd)).await?;

        {
            // Obtain the writer.
            // When the writer is dropped, EOF will be sent to
            // the spawned program.
            let writer = pair.master.take_writer()?;
            drop(writer);
        }

        let child_status = asupersync::runtime::spawn_blocking(move || {
            child
                .wait()
                .map_err(|e| anyhow!("waiting for child: {}", e))
        })
        .await?;
        println!("child status: {:?}", child_status);

        let reader = pair.master.try_clone_reader()?;

        // Take care to drop the master after our processes are done, as some
        // platforms get unhappy if it is dropped sooner than that.
        drop(pair.master);

        let output_lines =
            asupersync::runtime::spawn_blocking(move || -> anyhow::Result<Vec<String>> {
                let mut reader = std::io::BufReader::new(reader);
                let mut lines = Vec::new();
                let mut line = String::new();
                loop {
                    line.clear();
                    let bytes_read = reader
                        .read_line(&mut line)
                        .map_err(|e| anyhow!("problem reading line: {}", e))?;
                    if bytes_read == 0 {
                        break;
                    }
                    if line.ends_with('\n') {
                        line.pop();
                        if line.ends_with('\r') {
                            line.pop();
                        }
                    }
                    lines.push(line.clone());
                }
                Ok(lines)
            })
            .await?;

        for line in output_lines {
            // We print with escapes escaped because the windows conpty
            // implementation synthesizes title change escape sequences
            // in the output stream and it can be confusing to see those
            // printed out raw in another terminal.
            print!("output: len={} ", line.len());
            for c in line.escape_debug() {
                print!("{}", c);
            }
            println!();
        }

        Ok(())
    })
}
