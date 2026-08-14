use std::{
    fs::File,
    io::{self, BufRead as _, BufReader, Write as _},
    path::PathBuf,
    process::{Command, ExitCode, Stdio},
};

fn push_arg(file: &mut File, flag: &str, value: &str) -> io::Result<()> {
    // write_all_vectored is unstable, unfortunately. easiest to do this
    // flag files are utf-8, trailing newlines are fine
    file.write_all(format!("{}\n{}\n", flag, value).as_bytes())
}

fn main() -> ExitCode {
    let mut args = std::env::args_os();
    args.next().expect("more than 0 arguments");
    // nix derivations can't set cwd, do it ourselves
    let cwd = args.next().expect("crate root argument");

    let flags_dir: PathBuf = args.next().expect("flags directory argument").into();
    let out_dir: PathBuf = args.next().expect("out directory argument").into();
    let executable: PathBuf = args.next().expect("executable argument").into();

    std::fs::create_dir(&flags_dir).expect("creating flags directory");
    std::fs::create_dir(&out_dir).expect("creating out directory");

    // TODO: all the other link-arg-*
    let mut immediate_file = File::create_new(flags_dir.join("args-immediate")).unwrap();
    let mut transitive_file = File::create_new(flags_dir.join("args-transitive")).unwrap();
    // must be separate since --env-set is unstable
    let mut env_file = File::create_new(flags_dir.join("env")).unwrap();

    let mut error_msg = None;

    let mut child = Command::new(executable)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .current_dir(cwd)
        .env("OUT_DIR", &out_dir)
        .spawn()
        .expect("could not start process");

    let reader = BufReader::new(child.stdout.take().unwrap());

    for line in reader.lines() {
        // technically using a string here means we're limiting args,
        // but cargo also forces args to be valid utf-8
        let line = line.expect("Error reading line");
        println!("{}", line);
        let Some(rest) = line.strip_prefix("cargo:") else {
            continue;
        };
        // some commands work with either cargo:command or cargo::command, easiest to allow both
        let rest = rest.strip_prefix(":").unwrap_or(rest);

        let Some((key, value)) = rest.split_once('=') else {
            continue;
        };

        match key {
            "rustc-link-arg" => {
                push_arg(&mut transitive_file, "-C", &format!("link-arg={}", value))
            }
            "rustc-link-lib" => push_arg(&mut transitive_file, "-l", value),
            "rustc-link-search" => push_arg(&mut transitive_file, "-L", value),
            "rustc-cfg" => push_arg(&mut immediate_file, "--cfg", value),
            "rustc-check-cfg" => push_arg(&mut immediate_file, "--check-cfg", value),
            "rustc-flags" => transitive_file.write_all(value.replace(' ', "\n").as_bytes()),
            "rustc-env" => {
                // the lines format already disallows variables with newlines,
                // might as well use it as a separator
                env_file.write_all(format!("{}\n", value).as_bytes())
            }
            "error" => {
                error_msg = Some(value.to_string());
                Ok(())
            }
            // TODO: cargo-metadata
            _ => Ok(()),
        }
        .expect("could not write arguments");
    }

    let exit_code = child
        .wait()
        .expect("could not wait for process")
        .code()
        .unwrap_or(0);

    if let Some(msg) = error_msg {
        println!("Build script error: {}", msg);
        ExitCode::FAILURE
    } else {
        (exit_code as u8).into()
    }
}
