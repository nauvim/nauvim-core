use nvim_rpc_plugin::Error as RpcError;
use nvim_rpc_ui::{ClientError, Error as UiError};

use mlua::Error as LuaError;
#[cfg(feature = "ssh")]
use openrussh_client::connect::Error as SshConnectError;
#[cfg(feature = "ssh")]
use openrussh_client::known_hosts::KnownHostsHandler;
#[cfg(feature = "ssh")]
use openrussh_client::russh::Error as SshError;
#[cfg(feature = "ssh")]
use russh_sftp::client::error::Error as SftpError;
use tokio::task::JoinError;

use std::string::FromUtf8Error;
use std::{error, fmt, io};

/// A configuration, session, or transport operation failed.
#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    Lua(LuaError),
    Rpc(RpcError),
    Ui(UiError),
    Input(ClientError),
    Task(JoinError),
    #[cfg(feature = "ssh")]
    SshConnect(SshConnectError<KnownHostsHandler>),
    #[cfg(feature = "ssh")]
    Ssh(SshError),
    #[cfg(feature = "ssh")]
    Sftp(SftpError),
    Utf8(FromUtf8Error),
    /// The frontend rejected the selected UI-specific options.
    UiOptions(Box<Error>),
    /// Configuration loading failed and the frontend also rejected its defaults.
    DefaultUiOptions {
        configuration: Box<Error>,
        defaults: Box<Error>,
    },
    Message(String),
    Timeout(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => e.fmt(f),
            Self::Lua(e) => e.fmt(f),
            Self::Rpc(e) => e.fmt(f),
            Self::Ui(e) => e.fmt(f),
            Self::Input(e) => e.fmt(f),
            Self::Task(e) => e.fmt(f),
            #[cfg(feature = "ssh")]
            Self::SshConnect(e) => e.fmt(f),
            #[cfg(feature = "ssh")]
            Self::Ssh(e) => e.fmt(f),
            #[cfg(feature = "ssh")]
            Self::Sftp(e) => e.fmt(f),
            Self::Utf8(e) => e.fmt(f),
            Self::UiOptions(e) => write!(f, "Parse UI options: {e}"),
            Self::DefaultUiOptions {
                configuration,
                defaults,
            } => write!(
                f,
                "Parse default UI options after configuration failure ({configuration}): {defaults}"
            ),
            Self::Message(s) => f.write_str(s),
            Self::Timeout(s) => write!(f, "{s} timed out"),
        }
    }
}

impl error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}
impl From<LuaError> for Error {
    fn from(e: LuaError) -> Self {
        Self::Lua(e)
    }
}
impl From<RpcError> for Error {
    fn from(e: RpcError) -> Self {
        Self::Rpc(e)
    }
}
impl From<UiError> for Error {
    fn from(e: UiError) -> Self {
        Self::Ui(e)
    }
}
impl From<ClientError> for Error {
    fn from(e: ClientError) -> Self {
        Self::Input(e)
    }
}

impl From<JoinError> for Error {
    fn from(e: JoinError) -> Self {
        Self::Task(e)
    }
}

#[cfg(feature = "ssh")]
impl From<SshConnectError<KnownHostsHandler>> for Error {
    fn from(e: SshConnectError<KnownHostsHandler>) -> Self {
        Self::SshConnect(e)
    }
}

#[cfg(feature = "ssh")]
impl From<SshError> for Error {
    fn from(e: SshError) -> Self {
        Self::Ssh(e)
    }
}

#[cfg(feature = "ssh")]
impl From<SftpError> for Error {
    fn from(e: SftpError) -> Self {
        Self::Sftp(e)
    }
}

impl From<FromUtf8Error> for Error {
    fn from(e: FromUtf8Error) -> Self {
        Self::Utf8(e)
    }
}
