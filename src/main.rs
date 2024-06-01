// By: Eric MacDonald (eMac)

use std::env;
use getopts::Options;
use std::str;
use std::io::ErrorKind::{ConnectionReset, ConnectionAborted};
use futures::FutureExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::sync::broadcast;
use tokio::process::Command;
use std::process::Stdio;

#[macro_use]
extern crate counted_array;

struct StartCommand {
    program: String,
    params: getopts::Matches,
    getopts: Options,
}

struct StartOption {
    short_name: &'static str,
    long_name: &'static str,
    descirption: &'static str,
    example: &'static str,
    hint: &'static str,
    is_flag: bool,
}

const BUFFER_SIZE: usize = 0x1000;
const DEFAULT_IP: &'static str = "127.0.0.1";

counted_array!(static AVAILABLE_OPTIONS: [StartOption; _] = [
    StartOption {
        short_name: "l",
        long_name: "listen",
        descirption: "The socket address to listen on. This defaults to 127.0.0.1:1122. 127.0.0.1 is used as the IP if just the port is given.",
        example: "-l 6400",
        hint: "[HOST:]PORT",
        is_flag: false
    },
    StartOption {
        short_name: "c",
        long_name: "connect",
        descirption: "The remote server that provides PPP communication. This defaults to 127.0.0.1:2323.",
        example: "-c ppp.cool.com:2323",
        hint: "HOST:PORT",
        is_flag: false
    },
    StartOption {
        short_name: "e",
        long_name: "exec",
        descirption: "PPP command to run for direct PPP communication.",
        example: "-e '/usr/sbin/pppd notty'",
        hint: "'/path/to/exe exe_options'",
        is_flag: false
    },
    StartOption {
        short_name: "q",
        long_name: "silent",
        descirption: "Don't print anything unless it's a fatal exception. -h ignores this.",
        example: "",
        hint: "",
        is_flag: true
    },
    StartOption {
        short_name: "h",
        long_name: "help",
        descirption: "Print this help message",
        example: "",
        hint: "",
        is_flag: true
    },
]);

fn print_options(start_cmd: &StartCommand) -> Result<(), Box<dyn std::error::Error>> {
    let description = concat!(
        "WebTV Touch PPP v1.0.0: ",
        "Provides a way for the WebTV MAME driver to talk with PPP using its null modem.",
    );

    let epilog = concat!(
        "Special thanks to: Zefie, MattMan, and others in the WebTV hacking community!",
    );

    println!("{}\n", description);

    let brief = format!("Usage: {} [options]", start_cmd.program);

    print!("{}", start_cmd.getopts.usage(&brief));

    println!("\n{}", epilog);

    Ok(())
}

fn parse_options() -> Result<StartCommand, Box<dyn std::error::Error>> {
    let args: Vec<String> = env::args().collect();

    let mut getopts = Options::new();

    for option in AVAILABLE_OPTIONS.iter() {
        let description: String;

        if option.example != "" {
            description = format!("{}\nExample: {}", option.descirption, &option.example) ;
        } else {
            description = format!("{}", &option.descirption) ;
        }

        if option.is_flag {
            getopts.optflag(&option.short_name, &option.long_name, &description);
        } else {
            getopts.optopt(&option.short_name, &option.long_name, &description, &option.hint);
        }
    }

    let params = match getopts.parse(&args[1..]) {
        Ok(m) => { m }
        Err(f) => { panic!("{f}") }
    };

    Ok(StartCommand {
        program: args[0].clone(),
        params: params,
        getopts: getopts,
    })
}

async fn copy_loop<R, W>(
    read: &mut R,
    write: &mut W,
    mut abort: broadcast::Receiver<()>,
) -> tokio::io::Result<usize>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut copied_bytes = 0;
    let mut buf = [0u8; BUFFER_SIZE];
    loop {
        let bytes_found;
        tokio::select! {
            biased;

            result = read.read(&mut buf) => {
                bytes_found = result.or_else(|e| match e.kind() {
                    ConnectionReset | ConnectionAborted => Ok(0),
                    _ => Err(e)
                })?;
            },
            _ = abort.recv() => {
                break;
            }
        }

        if bytes_found == 0 {
            break;
        }

        //thread::sleep(time::Duration::from_millis(10));
        //println!("B:{:x?}", &buf[0..bytes_found]);

        write.write_all(&buf[0..bytes_found]).await?;
        copied_bytes += bytes_found;
    }

    Ok(copied_bytes)
}

async fn local_exec_loop(mame: &mut TcpStream, local_program_command: &String) -> Result<(usize, usize), Box<dyn std::error::Error>> {
    let (mut mame_reader, mut mame_writer) = mame.split();

    let mut the_args = local_program_command.split(' '); 
    let first: &str = the_args.next().unwrap();
    let rest: Vec<&str> = the_args.collect::<Vec<&str>>();

    println!("Got it? '{}'\n", first);
    println!("Got it2? '{}'\n", local_program_command);

    let mut ppp = match Command::new(first)
        .args(rest)
        .stdout(Stdio::piped())
        .stdin(Stdio::piped())
        .kill_on_drop(true)
        .spawn() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Unable to launch PPP! {e}");

            return Ok((0, 0));
        },
    };

    let mut ppp_reader = BufReader::new(ppp.stdout.take().expect("No PPP STDOUT?"));
    let mut ppp_writer = BufWriter::new(ppp.stdin.take().expect("No PPP STDIN?"));

    let (cancel, _) = broadcast::channel::<()>(1);

    let (ppp_to_mame_copied_bytes, mame_to_ppp_copied_bytes) = tokio::join!{
        copy_loop(&mut ppp_reader, &mut mame_writer, cancel.subscribe())
            .then(|r| { let _ = cancel.send(()); async { r } }),
        copy_loop(&mut mame_reader, &mut ppp_writer, cancel.subscribe())
            .then(|r| { let _ = cancel.send(()); async { r } }),
    };

    Ok((mame_to_ppp_copied_bytes.unwrap(), ppp_to_mame_copied_bytes.unwrap()))
}

async fn remote_ppp_loop(mame: &mut TcpStream, remote_socket_address: &String) -> Result<(usize, usize), Box<dyn std::error::Error>> {
    let mut ppp: TcpStream = match TcpStream::connect(remote_socket_address).await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Couldn't touch PPP: error={e}");

            return Ok((0, 0));
        }
    };

    let (mut mame_reader, mut mame_writer) = mame.split();
    let (mut ppp_reader, mut ppp_writer) = ppp.split();

    let (cancel, _) = broadcast::channel::<()>(1);

    let (ppp_to_mame_copied_bytes, mame_to_ppp_copied_bytes) = tokio::join!{
        copy_loop(&mut ppp_reader, &mut mame_writer, cancel.subscribe())
            .then(|r| { let _ = cancel.send(()); async { r } }),
        copy_loop(&mut mame_reader, &mut ppp_writer, cancel.subscribe())
            .then(|r| { let _ = cancel.send(()); async { r } }),
    };

    Ok((mame_to_ppp_copied_bytes.unwrap(), ppp_to_mame_copied_bytes.unwrap()))
}

//#[tokio::main(flavor = "multi_thread", worker_threads = 3)]
#[tokio::main]
async fn server_loop(start_cmd: &StartCommand) -> Result<(), Box<dyn std::error::Error>> {

    let mut listen_socket_address = format!("{}:{}", DEFAULT_IP, 1122);

    if start_cmd.params.opt_present("l") {
        listen_socket_address = start_cmd.params.opt_str("l")
            .expect("failed to resolve listen address");

        if !listen_socket_address.contains(":") {
            listen_socket_address = format!("{}:{}", DEFAULT_IP, listen_socket_address);
        }
    }

    let mut remote_socket_address = format!("{}:{}", DEFAULT_IP, 2323);
    if start_cmd.params.opt_present("c") {
        remote_socket_address = start_cmd.params.opt_str("c")
            .expect("failed to resolve remote address");

        if !listen_socket_address.contains(":") {
            remote_socket_address = format!("{}:{}", DEFAULT_IP, remote_socket_address);
        }
    }

    let mut local_program_command: String = "".to_string();
    if start_cmd.params.opt_present("e") {
        local_program_command = start_cmd.params.opt_str("e")
            .expect("failed to resolve remote address");
    }

    let listener = TcpListener::bind(&listen_socket_address).await?;

    println!("Listening on {listen_socket_address}.\n");

    println!("You need to add '-spot:modem null_modem -bitb socket.{listen_socket_address}' to the MAME command line.\n");

    loop {
        let (mut mame, mame_socket_address) = listener.accept().await?;

        let remote_socket_address = remote_socket_address.clone();
        let local_program_command = local_program_command.clone();

        tokio::spawn(async move {

            let mut buf = [0; BUFFER_SIZE];

            println!("Looks like we got a wild MAME @ {mame_socket_address}");

            let mut at_string: String = "".to_string();

            loop {
                let n: usize = match mame.read(&mut buf).await {
                    Ok(n) if n == 0 => return,
                    Ok(n) => n,
                    Err(e) => {
                        eprintln!("Can't listen to MAME: error={e}");
                        return;
                    }
                };

                if buf[0] >= 0x0a && buf[0] < 0x80 {
                    let s = String::from_utf8_lossy(&buf[0..n]);

                    at_string.push_str(&s);

                    print!("{}", s.replace("\x0d", "\x0a"));
                }

                // 79: CARRIER 33600
                // 67: COMPRESSION: V.42 bis
                // 19: CONECTED 115200

                if buf[n - 1] == 0x0d {
                    // Init string always turns echo off
                    if at_string.as_str().contains("E0") { // Init string
                        if let Err(e) = mame.write_all(b"OK\x0d\x0a").await {
                            eprintln!("Can't talk to MAME: error={e}");
                            return;
                        }
                    // Dial setup string usually doesn't have a phone number or echo value.
                    } else if !at_string.contains("E0") && !at_string.contains("DT") && !at_string.contains("TD") { // Dial setup string
                        // OK
                        if let Err(e) = mame.write_all(b"\x0d\x0a0\x0d\x0a").await {
                            eprintln!("Can't talk to MAME: error={e}");
                            return;
                        }
                    // DT in the string means a dial command.
                    } else if at_string.contains("DT") { // Dial string
                        if let Err(e) = mame.write_all(b"0\x0d\x0a").await {
                            eprintln!("Can't talk to MAME: error={e}");
                            return;
                        }

                    // ATD standalone is the request to go into data mode.
                    } else if at_string.contains("TD\x0d") { // ATD, go into data mode
                        if let Err(e) = mame.write_all(b"79\x0d\x0a67\x0d\x0a19\x0d\x0a").await {
                            eprintln!("Can't talk to MAME: error={e}");
                            return;
                        }

                        let mame_to_ppp_copied_bytes;
                        let ppp_to_mame_copied_bytes;

                        if local_program_command != "" {
                            println!("Launching then touching some PPP! '{}'", local_program_command);

                            (mame_to_ppp_copied_bytes, ppp_to_mame_copied_bytes) = match local_exec_loop(&mut mame, &local_program_command).await {
                                Ok(r) => r,
                                Err(e) => {
                                    eprintln!("Error in remote PPP loop: error={e}");
                                    return;
                                }
                            };
                        } else {
                            println!("Touching PPP! '{}'", remote_socket_address);

                            (mame_to_ppp_copied_bytes, ppp_to_mame_copied_bytes) = match remote_ppp_loop(&mut mame, &remote_socket_address).await {
                                Ok(r) => r,
                                Err(e) => {
                                    eprintln!("Error in remote PPP loop: error={e}");
                                    return;
                                }
                            };
                        }

                        println!("Looks like the MAME is done? Taking my hands off PPP. {mame_to_ppp_copied_bytes} bytes copied from MAME to PPP; {ppp_to_mame_copied_bytes} bytes copied from PPP to MAME\n");
                    }

                    at_string = "".to_string();
                }
            }
        });
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let start_cmd = match parse_options() {
        Ok(r) => r,
        Err(e) => return Err(e)
    };

    if start_cmd.params.opt_present("h") {
        match print_options(&start_cmd) {
            Ok(r) => r,
            Err(e) => return Err(e)
        };
    } else {
        match server_loop(&start_cmd) {
            Ok(r) => r,
            Err(e) => return Err(e)
        };
    }

    Ok(())
}