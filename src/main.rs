mod cmd;
mod ftp;
mod error;
mod codec;

use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::prelude::*;

use futures::{Sink, Stream, StreamExt};
use futures::stream::SplitSink;
use futures::prelude::*;
use tokio::io::AsyncRead;
use tokio_util::codec::Framed;

use crate::codec::FtpCodec;
use crate::error::{Error, Result};
use crate::ftp::{Answer, ResultCode};
use crate::cmd::Command;

use std::path::PathBuf;

type Writer = SplitSink<Framed<TcpStream, FtpCodec>, Answer>;

struct Client {
    cwd: PathBuf,
    writer: Writer,
}

impl Client {
    fn new(writer: Writer) -> Client {
        Client {
            cwd: PathBuf::from("/"),
            writer,
        }
    }

    async fn handle_cmd(mut self, cmd: Command) -> Result<Self> {
        println!("Received command: {:?}", cmd);
        match cmd {
            Command::User(content) => {
                if content.is_empty() {
                    self = self.send(Answer::new(ResultCode::InvalidParameterOrArgument, "Invalid username")).await?;
                } else {
                    self = self.send(Answer::new(ResultCode::UserLoggedIn, &format!("Welcome {}!", content))).await?;
                }
            },
            Command::Pwd => {
                let msg = format!("{}", self.cwd.to_str().unwrap_or(""));
                if ! msg.is_empty() {
                    let message = format!("\"/{}\" ", msg);
                    self = self.send(Answer::new(ResultCode::PATHNAMECreated, &message)).await?;
                } else {
                    self = self.send(Answer::new(ResultCode::FileNotFound, "No such file or directory")).await?;
                }
            },
            Command::Cwd(directory) => self = self.cwd(directory).await?,
            Command::Unknown(s) => {
                self = self.send(Answer::new(ResultCode::UnknownCommand, &format!("\"{}\": Not implemented", s))).await?
            },
            _ => self = self.send(Answer::new(ResultCode::CommandNotImplemented, "Not implemented")).await?,
        }
        Ok(self)
    }

    async fn send(mut self, answer: Answer) -> Result<Self> {
        self.writer.send(answer).await?;
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
                self = self.send(Answer::new(ResultCode::Ok, &format!("Directory changed to \"{}\"", directory.display()))).await?;
                return Ok(self)
            }
        }
        self = self.send(Answer::new(ResultCode::FileNotFound, "No such file or directory")).await?;
        Ok(self)
    }

    fn complete_path(self, path: PathBuf) -> (Self, result::Result<PathBuf, io::Error) {
        
    }
}

#[tokio::main]
async fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let addr = "127.0.0.1:1234";
    let mut listener = TcpListener::bind(addr).await?;

    loop {
        let (mut socket, addr) = listener.accept().await?;

        let address = format!("[address: {}]", addr);
        println!("New client: {}", address);

        tokio::spawn(async move {
            client(socket).await
        });
    }
}

async fn client(stream: TcpStream) -> io::Result<()> {
    let framed = Framed::new(stream, FtpCodec);
    let (mut writer, mut reader) = framed.split();
    // let (writer, reader) = stream.framed(FtpCodec).split();
    writer.send(Answer::new(ResultCode::ServiceReadyForNewUser, "Welcome to this FTP server!")).await?;
    let mut client = Client::new(writer);

    while let Some(cmd) = reader.next().await {
        client = match cmd {
            Ok(cmd) => client.handle_cmd(cmd).await?,
            Err(e) => { eprintln!("get cmd error: {}", e); client },
        }
    }
    
    // for cmd in reader {
    //     client = client.handle_cmd(cmd).await?;
    // }
    Ok(())
}