#![forbid(unsafe_code)]

use std::env;
use std::fs;
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::process;
use std::thread;
use std::time::Duration;

fn main() -> io::Result<()> {
    let mut arguments = env::args_os().skip(1);
    let mode = arguments.next().expect("the golden probe needs a mode");
    let mut stdout = io::stdout().lock();
    match mode.to_str().expect("the mode is UTF-8") {
        "args" => {
            for argument in arguments {
                write!(stdout, "arg:")?;
                for byte in argument.as_bytes() {
                    write!(stdout, "{byte:02x}")?;
                }
                writeln!(stdout)?;
            }
        }
        "env" => {
            let name = arguments.next().expect("env needs a variable name");
            let value = env::var_os(name).expect("the golden environment value is present");
            stdout.write_all(value.as_bytes())?;
            stdout.write_all(b"\n")?;
        }
        "pwd" => {
            stdout.write_all(env::current_dir()?.as_os_str().as_bytes())?;
            stdout.write_all(b"\n")?;
        }
        "emit" => {
            stdout.write_all(b"pipeline bytes\n")?;
            io::stderr().lock().write_all(b"probe stderr\n")?;
        }
        "emit-large" => {
            let out = [b'O'; 8192];
            let err = [b'E'; 8192];
            let mut stderr = io::stderr().lock();
            for _ in 0..1024 {
                stdout.write_all(&out)?;
                stderr.write_all(&err)?;
            }
            stdout.write_all(b"O")?;
            stderr.write_all(b"E")?;
        }
        "copy" => {
            io::copy(&mut io::stdin().lock(), &mut stdout)?;
        }
        "status" => {
            let code = arguments
                .next()
                .expect("status needs a code")
                .to_str()
                .expect("status code is UTF-8")
                .parse()
                .expect("status code is an integer");
            process::exit(code);
        }
        "mark" => {
            let path = arguments.next().expect("mark needs a path");
            fs::write(path, b"started")?;
        }
        "hold" => {
            let path = arguments.next().expect("hold needs a path");
            fs::write(path, process::id().to_string())?;
            loop {
                thread::sleep(Duration::from_secs(1));
            }
        }
        _ => panic!("unknown golden probe mode"),
    }
    Ok(())
}
