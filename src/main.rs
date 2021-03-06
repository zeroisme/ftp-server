mod cmd;
mod codec;
mod error;
mod ftp;
mod config;

#[macro_use]
extern crate serde_derive;

use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tokio::prelude::*;
use tokio::fs::File;
use tokio::io::AsyncWriteExt;

use crate::cmd::{Command, TransferType};
use crate::codec::FtpCodec;
use crate::error::{Error, Result};
use crate::ftp::{Answer, ResultCode};
use futures::prelude::*;
use futures::stream::SplitSink;
use futures::stream::SplitStream;
use futures::{StreamExt};
use tokio_util::codec::Framed;

use std::env;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::Path;
use std::path::PathBuf;
use std::path::StripPrefixError;
use std::result;

use std::fs::create_dir;
use std::fs::read_dir;
use std::fs::remove_dir_all;
use std::path::Component;

use crate::config::Config;
use crate::config::DEFAULT_PORT;

const CONFIG_FILE: &'static str = "config.toml";

fn invalid_path(path: &Path) -> bool {
    for component in path.components() {
        if let Component::ParentDir = component {
            return true;
        }
    }
    false
}

fn prefix_slash(path: &mut PathBuf) {
    if !path.is_absolute() {
        *path = Path::new("/").join(&path);
    }
}

use crate::codec::BytesCodec;

type DataReader = SplitStream<Framed<TcpStream, BytesCodec>>;
type DataWriter = SplitSink<Framed<TcpStream, BytesCodec>, Vec<u8>>;
type Writer = SplitSink<Framed<TcpStream, FtpCodec>, Answer>;

use std::ffi::OsString;

use std::fs::Metadata;
#[cfg(windows)]
fn get_file_info(meta: &Metadata) -> (time::Tm, u64) {
    use std::os::windows::prelude::*;
    (
        time::at(time::Timespec::new(meta.last_write_time())),
        meta.file_size(),
    )
}
#[cfg(not(windows))]
fn get_file_info(meta: &Metadata) -> (time::Tm, u64) {
    use std::os::unix::prelude::*;
    (time::at(time::Timespec::new(meta.mtime(), 0)), meta.size())
}

fn get_parent(path: PathBuf) -> Option<PathBuf> {
    path.parent().map(|p| p.to_path_buf())
}

fn get_filename(path: PathBuf) -> Option<OsString> {
    path.file_name().map(|p| p.to_os_string())
}

struct Client {
    data_port: Option<u16>,
    data_reader: Option<DataReader>,
    data_writer: Option<DataWriter>,
    cwd: PathBuf,
    name: Option<String>,
    server_root: PathBuf,
    transfer_type: TransferType,
    writer: Writer,
    is_admin: bool,
    config: Config, 
    waiting_password: bool,
}

impl Client {
    fn new(writer: Writer, server_root: PathBuf, config: Config) -> Client {
        Client {
            data_port: None,
            data_reader: None,
            data_writer: None,
            cwd: PathBuf::from("/"),
            name: None,
            server_root,
            transfer_type: TransferType::Ascii,
            writer,
            is_admin: false,
            config,
            waiting_password: false,
        }
    }

    async fn handle_cmd(mut self, cmd: Command) -> Result<Self> {
        println!("Received command: {:?}", cmd);

        if self.is_logged() {
            match cmd {
                Command::Cwd(directory) => return Ok(self.cwd(directory).await?),
                Command::List(path) => return Ok(self.list(path).await?),
                Command::Pasv => return Ok(self.pasv().await?),
                Command::Port(port) => {
                    self.data_port = Some(port);
                    return Ok(self.send(Answer::new(ResultCode::Ok, &format!("Data port is now {}", port))).await?);
                },
                Command::Pwd => {
                    let msg = format!("{}", self.cwd.to_str().unwrap_or(""));
                    if !msg.is_empty() {
                        let message = format!("\"{}\" ", msg);
                        return Ok(self.send(Answer::new(ResultCode::PATHNAMECreated, &message)).await?);
                    } else {
                        return Ok(self.send(Answer::new(ResultCode::FileNotFound, "No such file or directory")).await?);
                    }
                },
                Command::Retr(file) => return Ok(self.retr(file).await?),
                Command::Stor(file) => return Ok(self.stor(file).await?),
                Command::CdUp => {
                    if let Some(path) = self.cwd.parent().map(Path::to_path_buf) {
                        self.cwd = path;
                        prefix_slash(&mut self.cwd);
                    }
                    return Ok(self.send(Answer::new(ResultCode::Ok, "Done")).await?);
                },
                Command::Mkd(path) => return Ok(self.mkd(path).await?),
                Command::Rmd(path) => return Ok(self.rmd(path).await?),
                _ => (),
            }
        } else if self.name.is_some() && self.waiting_password {
            if let Command::Pass(content) = cmd {
                let mut ok = false;
                if self.is_admin {
                    ok = content == self.config.admin.as_ref().unwrap().password;
                } else {
                    for user in &self.config.users {
                        if Some(&user.name) == self.name.as_ref() {
                            if user.password == content {
                                ok = true;
                                break;
                            }
                        }
                    }
                }
                if ok {
                    self.waiting_password = false;
                    let name = self.name.clone().unwrap_or(String::new());
                    self = self.send(Answer::new(ResultCode::UserLoggedIn, &format!("Welcome {}", name))).await?;
                } else {
                    self = self.send(Answer::new(ResultCode::NotLoggedIn, "Invalid password")).await?;
                }

                return Ok(self);
            }
        }
        match cmd {
            Command::User(content) => {
                if content.is_empty() {
                    self = self
                        .send(Answer::new(
                            ResultCode::InvalidParameterOrArgument,
                            "Invalid username",
                        ))
                        .await?;
                } else {
                    let mut name = None;
                    let mut pass_required = true;

                    self.is_admin = false;
                    if let Some(ref admin) = self.config.admin {
                        if admin.name == content {
                            pass_required = admin.password.is_empty() == false;
                            self.is_admin = true;
                        }
                    }

                    // In case the user isn't the admin
                    if name.is_none() {
                        for user in &self.config.users {
                            if user.name == content {
                                name = Some(content.clone());
                                pass_required = user.password.is_empty() == false;
                                break;
                            }
                        }
                    }
                    // In case this is an unknown user.
                    if name.is_none() {
                        self = self.send(Answer::new(ResultCode::NotLoggedIn, "Unknown user...")).await?;
                    } else {
                        self.name = name.clone();
                        if pass_required {
                            self.waiting_password = true;
                            self = self.send(Answer::new(ResultCode::UserNameOkayNeedPassword, &format!("Login Ok password needed for {}", name.unwrap()))).await?;
                        } else {
                            self.waiting_password = false;
                            self = self.send(Answer::new(ResultCode::UserLoggedIn, &format!("Welcome {}!", content))).await?;
                        }
                    }
                }
            }
            Command::NoOp => self = self.send(Answer::new(ResultCode::Ok, "Doing nothing")).await?,
            Command::Type(typ) => {
                self.transfer_type = typ;
                self = self
                    .send(Answer::new(
                        ResultCode::Ok,
                        "Transfer type changed successfully",
                    ))
                    .await?;
            }
            Command::Syst => {
                self = self.send(Answer::new(ResultCode::Ok, "I won't tell!")).await?;
            }
            Command::Unknown(s) => {
                self = self
                    .send(Answer::new(
                        ResultCode::UnknownCommand,
                        &format!("\"{}\": Not implemented", s),
                    ))
                    .await?
            }
            Command::Quit => self = self.quit().await?,
            _ => {
                // Not Logged in
                self = self
                    .send(Answer::new(
                        ResultCode::NotLoggedIn,
                        "Please log first",
                    ))
                    .await?
            }
        }
        Ok(self)
    }

    async fn send(mut self, answer: Answer) -> Result<Self> {
        self.writer.send(answer).await?;
        Ok(self)
    }

    async fn pasv(mut self) -> Result<Self> {
        let port = if let Some(port) = self.data_port {
            port
        } else {
            0
        };

        if self.data_writer.is_some() {
            self = self
                .send(Answer::new(
                    ResultCode::DataConnectionAlreadyOpen,
                    "Already listening...",
                ))
                .await?;
            return Ok(self);
        }
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), port);
        let mut listener = TcpListener::bind(addr).await?;
        let port = listener.local_addr()?.port();
        self = self
            .send(Answer::new(
                ResultCode::EnteringPassiveMode,
                &format!("127,0,0,1,{},{}", port >> 8, port & 0xFF),
            ))
            .await?;
        println!("Waiting clients on port {}...", port);

        let (socket, addr) = listener.accept().await?;
        println!("Address: {}", addr);
        let (writer, reader) = Framed::new(socket, BytesCodec).split();
        self.data_writer = Some(writer);
        self.data_reader = Some(reader);

        Ok(self)
    }

    async fn cwd(mut self, directory: PathBuf) -> Result<Self> {
        let path = self.cwd.join(&directory);
        let (new_self, res) = self.complete_path(path);
        self = new_self;
        if let Ok(dir) = res {
            let (new_self, res) = self.strip_prefix(dir);
            self = new_self;
            if let Ok(prefix) = res {
                self.cwd = prefix.to_path_buf();
                self = self
                    .send(Answer::new(
                        ResultCode::RequestedFileActionOkay,
                        &format!("Directory changed to \"{}\"", directory.display()),
                    ))
                    .await?;
                return Ok(self);
            }
        }
        self = self
            .send(Answer::new(
                ResultCode::FileNotFound,
                "No such file or directory",
            ))
            .await?;
        Ok(self)
    }

    fn complete_path(self, path: PathBuf) -> (Self, result::Result<PathBuf, io::Error>) {
        let directory = self.server_root.join(if path.has_root() {
            path.iter().skip(1).collect()
        } else {
            path
        });

        let dir = directory.canonicalize();
        if let Ok(ref dir) = dir {
            if !dir.starts_with(&self.server_root) {
                return (self, Err(io::ErrorKind::PermissionDenied.into()));
            }
        }
        (self, dir)
    }

    fn strip_prefix(self, dir: PathBuf) -> (Self, result::Result<PathBuf, StripPrefixError>) {
        let res = dir.strip_prefix(&self.server_root).map(|p| p.to_path_buf());
        (self, res)
    }

    async fn quit(mut self) -> Result<Self> {
        if self.data_writer.is_some() {
            unimplemented!();
        } else {
            self = self
                .send(Answer::new(
                    ResultCode::ServiceClosingControlConnection,
                    "Closing connection...",
                ))
                .await?;
            self.writer.close().await?;
        }
        Ok(self)
    }

    async fn mkd(mut self, path: PathBuf) -> Result<Self> {
        let path = self.cwd.join(&path);
        let parent = get_parent(path.clone());
        if let Some(parent) = parent {
            let parent = parent.to_path_buf();
            let (new_self, res) = self.complete_path(parent);
            self = new_self;
            if let Ok(mut dir) = res {
                if dir.is_dir() {
                    let filename = get_filename(path);
                    if let Some(filename) = filename {
                        dir.push(filename);
                        if create_dir(dir).is_ok() {
                            self = self
                                .send(Answer::new(
                                    ResultCode::PATHNAMECreated,
                                    "Folder successfully created!",
                                ))
                                .await?;
                            return Ok(self);
                        }
                    }
                }
            }
        }
        self = self
            .send(Answer::new(
                ResultCode::FileNotFound,
                "Couldn't create folder",
            ))
            .await?;
        Ok(self)
    }

    async fn rmd(mut self, directory: PathBuf) -> Result<Self> {
        let path = self.cwd.join(&directory);
        let (new_self, res) = self.complete_path(path);
        self = new_self;
        if let Ok(dir) = res {
            if remove_dir_all(dir).is_ok() {
                self = self
                    .send(Answer::new(
                        ResultCode::RequestedFileActionOkay,
                        "successfully removed",
                    ))
                    .await?;
                return Ok(self);
            }
        }
        self = self
            .send(Answer::new(
                ResultCode::FileNotFound,
                "Couldn't remove folder",
            ))
            .await?;
        Ok(self)
    }

    async fn list(mut self, path: Option<PathBuf>) -> Result<Self> {
        if self.data_writer.is_some() {
            let path = self.cwd.join(path.unwrap_or_default());
            let directory = PathBuf::from(&path);

            let (new_self, res) = self.complete_path(directory);
            self = new_self;
            if let Ok(path) = res {
                self = self
                    .send(Answer::new(
                        ResultCode::DataConnectionAlreadyOpen,
                        "Starting to list directory...",
                    ))
                    .await?;

                let mut out = vec![];
                if path.is_dir() {
                    if let Ok(dir) = read_dir(path) {
                        for entry in dir {
                            if let Ok(entry) = entry {
                                if self.is_admin || entry.path() != self.server_root.join(CONFIG_FILE) {
                                    add_file_info(entry.path(), &mut out);
                                }
                                
                            }
                        }
                    } else {
                        self = self
                            .send(Answer::new(
                                ResultCode::InvalidParameterOrArgument,
                                "No such file or directory",
                            ))
                            .await?;
                        return Ok(self);
                    }
                } else {
                    if self.is_admin || path != self.server_root.join(CONFIG_FILE) {
                        add_file_info(path, &mut out);
                    }
                }
                self = self.send_data(out).await?;
                println!("-> and done");
            } else {
                self = self
                    .send(Answer::new(
                        ResultCode::InvalidParameterOrArgument,
                        "No such file or directory",
                    ))
                    .await?;
            }
            if self.data_writer.is_some() {
                self.close_data_connection();
                self = self
                    .send(Answer::new(
                        ResultCode::ClosingDataConnection,
                        "Transfer done",
                    ))
                    .await?;
            }
        } else {
            self = self
                .send(Answer::new(
                    ResultCode::ConnectionClosed,
                    "No opened data connection",
                ))
                .await?;
        }
        Ok(self)
    }

    async fn send_data(mut self, data: Vec<u8>) -> Result<Self> {
        if let Some(mut writer) = self.data_writer {
            writer.send(data).await?;
            self.data_writer = Some(writer);
        }
        Ok(self)
    }

    fn close_data_connection(&mut self) {
        self.data_reader = None;
        self.data_writer = None;
    }

    async fn retr(mut self, path: PathBuf) -> Result<Self> {
        if self.data_writer.is_some() {
            let path = self.cwd.join(path);
            let (new_self, res) = self.complete_path(path.clone());
            self = new_self;
            if let Ok(path) = res {
                if path.is_file() && (self.is_admin || path != self.server_root.join(CONFIG_FILE)) {
                    self = self
                        .send(Answer::new(
                            ResultCode::DataConnectionAlreadyOpen,
                            "Starting to send file...",
                        ))
                        .await?;
                    let mut file = File::open(path).await?;
                    let mut out = vec![];
                    file.read_to_end(&mut out).await?;
                    self = self.send_data(out).await?;
                    println!("-> file transfer done!");
                } else {
                    self = self
                        .send(Answer::new(
                            ResultCode::LocalErrorInProcessing,
                            &format!(
                                "\"{}\" doesn't exit",
                                path.to_str()
                                    .ok_or_else(|| Error::Msg("No path".to_string()))?
                            ),
                        ))
                        .await?;
                }
            } else {
                self = self
                    .send(Answer::new(
                        ResultCode::LocalErrorInProcessing,
                        &format!(
                            "\"{}\" doesn't exist",
                            path.to_str()
                                .ok_or_else(|| Error::Msg("No path".to_string()))?
                        ),
                    ))
                    .await?;
            }
        } else {
            self = self
                .send(Answer::new(
                    ResultCode::ConnectionClosed,
                    "No opened data connection",
                ))
                .await?;
        }
        if self.data_writer.is_some() {
            self.close_data_connection();
            self = self
                .send(Answer::new(
                    ResultCode::ClosingDataConnection,
                    "Transfer done",
                ))
                .await?;
        }
        Ok(self)
    }

    async fn stor(mut self, path: PathBuf) -> Result<Self> {
        if self.data_reader.is_some() {
            if invalid_path(&path) || (!self.is_admin && path == self.server_root.join(CONFIG_FILE)) {
                let error: io::Error = io::ErrorKind::PermissionDenied.into();
                return Err(error.into());
            }

            let path = self.cwd.join(path);
            self = self
                .send(Answer::new(
                    ResultCode::DataConnectionAlreadyOpen,
                    "Starting to send file...",
                ))
                .await?;
            let (data, new_self) = self.receive_data().await?;
            self = new_self;
            let mut file = File::create(path).await?;
            file.write_all(&data).await?;
            println!("-> file transfer done!");
            self.close_data_connection();
            self = self
                .send(Answer::new(
                    ResultCode::ClosingDataConnection,
                    "Transfer done",
                ))
                .await?;
        } else {
            self = self
                .send(Answer::new(
                    ResultCode::ConnectionClosed,
                    "No opened data connection",
                ))
                .await?;
        }
        Ok(self)
    }

    async fn receive_data(mut self) -> Result<(Vec<u8>, Self)> {
        let mut file_data = vec![];
        if self.data_reader.is_none() {
            return Ok((vec![], self));
        }

        let mut reader = self
            .data_reader
            .take()
            .ok_or_else(|| Error::Msg("No data reader".to_string()))?;

        while let Some(data) = reader.next().await {
            match data {
                Ok(data) => file_data.extend(&data),
                Err(e) => {
                    eprintln!("get cmd error: {}", e);
                }
            }
        }
    
        Ok((file_data, self))
    }

    fn is_logged(&self) -> bool {
        self.name.is_some() && !self.waiting_password
    }
}

#[tokio::main]
async fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let config = Config::new(CONFIG_FILE).expect("Error while lodding config...");
    let server_root = env::current_dir()?;
    server(server_root, config).await?;
    Ok(())
}

async fn server(server_root: PathBuf, config: Config) -> io::Result<()> {
    let port = config.server_port.unwrap_or(DEFAULT_PORT);
    let addr = SocketAddr::new(IpAddr::V4(config.server_addr.as_ref().unwrap_or(&"127.0.0.1".to_owned()).parse().expect("Invalid Ipv4 address...")), port);
    // let addr = "127.0.0.1:1234";
    let mut listener = TcpListener::bind(addr).await?;

    loop {
        let (socket, addr) = listener.accept().await?;

        let address = format!("[address: {}]", addr);
        println!("New client: {}", address);
        let server_root_copy = server_root.clone();
        let config_copy = config.clone();
        tokio::spawn(async move { handle_client(socket, server_root_copy, config_copy).await });
    }
}

async fn handle_client(
    stream: TcpStream,
    server_root: PathBuf,
    config: Config,
) -> result::Result<(), ()> {
    client(stream, server_root, config)
        .await
        .map_err(|error| println!("Error handling client: {}", error))
}

async fn client(stream: TcpStream, server_root: PathBuf, config: Config) -> io::Result<()> {
    let framed = Framed::new(stream, FtpCodec);
    let (mut writer, mut reader) = framed.split();
    // let (writer, reader) = stream.framed(FtpCodec).split();
    writer
        .send(Answer::new(
            ResultCode::ServiceReadyForNewUser,
            "Welcome to this FTP server!",
        ))
        .await?;
    let mut client = Client::new(writer, server_root, config);

    while let Some(cmd) = reader.next().await {
        client = match cmd {
            Ok(cmd) => client.handle_cmd(cmd).await?,
            Err(e) => {
                eprintln!("get cmd error: {}", e);
                client
            }
        }
    }

    Ok(())
}

const MONTHS: [&'static str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

fn add_file_info(path: PathBuf, out: &mut Vec<u8>) {
    let extra = if path.is_dir() { "/" } else { "" };
    let is_dir = if path.is_dir() { "d" } else { "-" };
    let meta = match ::std::fs::metadata(&path) {
        Ok(meta) => meta,
        _ => return,
    };
    let (time, file_size) = get_file_info(&meta);
    let path = match path.to_str() {
        Some(path) => match path.split("/").last() {
            Some(path) => path,
            _ => return,
        },
        _ => return,
    };
    let rights = if meta.permissions().readonly() {
        "r--r--r--"
    } else {
        "rw-rw-rw-"
    };

    let file_str = format!(
        "{is_dir}{rights} {links} {owner} {group} {size} {month} {day} {hour}:{min} {path}{extra}\r\n",
        is_dir = is_dir,
        rights = rights,
        links = 1,           // number of links
        owner = "anonymous", // owner name
        group = "anonymous", // group name
        size = file_size,
        month = MONTHS[time.tm_mon as usize],
        day = time.tm_mday,
        hour = time.tm_hour,
        min = time.tm_min,
        path = path,
        extra = extra
    );
    out.extend(file_str.as_bytes());
    println!("==> {:?}", &file_str);
}
