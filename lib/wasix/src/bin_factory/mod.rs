use std::{
    collections::HashMap,
    future::Future,
    ops::Deref,
    path::Path,
    pin::Pin,
    sync::{Arc, RwLock},
};

use anyhow::Context;
use shared_buffer::OwnedBuffer;
use virtual_fs::{AsyncReadExt, FileSystem};
use wasmer::FunctionEnvMut;
use wasmer_package::utils::from_bytes;

mod binary_package;
mod exec;

pub use self::{
    binary_package::*,
    exec::{
        package_command_by_name, run_exec, spawn_exec, spawn_exec_module, spawn_exec_wasm,
        spawn_load_module, spawn_union_fs,
    },
};
use crate::{
    os::{command::Commands, task::TaskJoinHandle},
    runtime::module_cache::HashedModuleData,
    Runtime, SpawnError, WasiEnv,
};

#[derive(Debug, Clone)]
pub struct BinFactory {
    pub(crate) commands: Commands,
    runtime: Arc<dyn Runtime + Send + Sync + 'static>,
    pub(crate) local: Arc<RwLock<HashMap<String, Option<BinaryPackage>>>>,
}

impl BinFactory {
    pub fn new(runtime: Arc<dyn Runtime + Send + Sync + 'static>) -> BinFactory {
        BinFactory {
            commands: Commands::new_with_builtins(runtime.clone()),
            runtime,
            local: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn runtime(&self) -> &(dyn Runtime + Send + Sync) {
        self.runtime.deref()
    }

    pub fn set_binary(&self, name: &str, binary: BinaryPackage) {
        let mut cache = self.local.write().unwrap();
        cache.insert(name.to_string(), Some(binary));
    }

    /*
    #[allow(clippy::await_holding_lock)]
    pub async fn get_binary(
        &self,
        name: &str,
        fs: Option<&dyn FileSystem>,
    ) -> Option<BinaryPackage> {
        self.get_executable(name, fs)
            .await
            .and_then(|executable| match executable {
                Executable::Wasm(_) => None,
                Executable::BinaryPackage(pkg) => Some(pkg),
            })
    }
    */

    pub fn spawn<'a>(
        &'a self,
        name: String,
        mut env: WasiEnv,
    ) -> Pin<Box<dyn Future<Output = Result<TaskJoinHandle, SpawnError>> + 'a>> {
        Box::pin(async move {
            // Find the binary (or die trying) and make the spawn type
            let res = self
                .get_executable(name.as_str(), Some(env.fs_root()))
                .await
                .ok_or_else(|| SpawnError::BinaryNotFound {
                    binary: name.clone(),
                });
            let (executable, mut pre_args) = res?;

            if !pre_args.is_empty() {
                let mut state = env.state.fork();
                pre_args.push(name.clone());  // replace argv[0] with full path
                state.args.get_mut().unwrap().splice(0..1, pre_args.drain(..));
                env.state = Arc::new(state);
            }

            // Execute
            match executable {
                Executable::Wasm(bytes) => {
                    let data = HashedModuleData::new(bytes.clone());
                    spawn_exec_wasm(data, name.as_str(), env, &self.runtime).await
                }
                Executable::BinaryPackage(pkg) => {
                    // Get the command that is going to be executed
                    let cmd = package_command_by_name(&pkg, name.as_str())?;

                    env.prepare_spawn(cmd);

                    spawn_exec(pkg, name.as_str(), env, &self.runtime).await
                }
            }
        })
    }

    pub fn try_built_in(
        &self,
        name: String,
        parent_ctx: Option<&FunctionEnvMut<'_, WasiEnv>>,
        builder: &mut Option<WasiEnv>,
    ) -> Result<TaskJoinHandle, SpawnError> {
        // We check for built in commands
        if let Some(parent_ctx) = parent_ctx {
            if self.commands.exists(name.as_str()) {
                return self.commands.exec(parent_ctx, name.as_str(), builder);
            }
        } else if self.commands.exists(name.as_str()) {
            tracing::warn!("builtin command without a parent ctx - {}", name);
        }
        Err(SpawnError::BinaryNotFound { binary: name })
    }

    // TODO: remove allow once BinFactory is refactored
    // currently fine because a BinFactory is only used by a single process tree
    #[allow(clippy::await_holding_lock)]
    pub async fn get_executable(
        &self,
        name: &str,
        fs: Option<&dyn FileSystem>,
    ) -> Option<(Executable, Vec<String>)> {
        let name = name.to_string();

        // Fast path
        {
            let cache = self.local.read().unwrap();
            if let Some(data) = cache.get(&name) {
                data.clone().map(Executable::BinaryPackage);
            }
        }

        // Slow path
        let mut cache = self.local.write().unwrap();

        // Check the cache
        if let Some(data) = cache.get(&name) {
            return data.clone().map(|pkg| (Executable::BinaryPackage(pkg), vec![]));
        }

        // Check the filesystem for the file
        if name.starts_with('/') {
            if let Some(fs) = fs {
                match load_executable_from_filesystem(fs, name.as_ref(), self.runtime()).await {
                    Ok((executable, pre_args)) => {
                        if let Executable::BinaryPackage(pkg) = &executable {
                            cache.insert(name, Some(pkg.clone()));
                        }

                        return Some((executable, pre_args));
                    }
                    Err(e) => {
                        tracing::warn!(
                            path = name,
                            error = &*e,
                            "Unable to load the package from disk"
                        );
                    }
                }
            }
        }

        // NAK
        cache.insert(name, None);
        None
    }
}

pub enum Executable {
    Wasm(OwnedBuffer),
    BinaryPackage(BinaryPackage),
}

async fn load_executable_from_filesystem(
    fs: &dyn FileSystem,
    path: &Path,
    rt: &(dyn Runtime + Send + Sync),
) -> Result<(Executable, Vec<String>), anyhow::Error> {
    let mut f = fs
        .new_open_options()
        .read(true)
        .open(path)
        .context("Unable to open the file")?;

    // Fast path if the file is fully available in memory.
    // Prevents redundant copying of the file data.
    let obuf: Option<OwnedBuffer> = f.as_owned_buffer();
    let bytes: Option<bytes::Bytes> = if obuf.is_some() { None } else {
        let mut data = Vec::with_capacity(f.size() as usize);
        f.read_to_end(&mut data).await.context("Read failed")?;
        Some(data.into())
    };
    let bytes_slice = &obuf.as_ref().map(|buf| &buf[..])
        .unwrap_or_else(|| &bytes.as_ref().unwrap()[..]);

    if let Some((exec, pre_args)) = Box::pin(shebang(fs, bytes_slice, rt)).await? {
        Ok((exec, pre_args))
    }
    else if let Some(container) = container_from_slice(bytes_slice) {
        let pkg = BinaryPackage::from_webc(&container, rt)
            .await
            .context("Unable to load the package")?;

        Ok((Executable::BinaryPackage(pkg), vec![]))
    }
    else {
        let buf = obuf.unwrap_or_else(|| OwnedBuffer::from_bytes(bytes.unwrap()));
        Ok((Executable::Wasm(buf), vec![]))
    }
}

fn container_from_slice(bytes: &[u8]) -> Option<webc::Container> {
    // can use a let-chain when Rust edition is bumped to 2024
    if wasmer_package::utils::is_container(bytes) {
        if let Ok(container) = from_bytes(bytes.to_vec()) {
            return Some(container);
        }
    }
    None
}

async fn shebang(fs: &dyn FileSystem, bytes: &[u8], rt: &(dyn Runtime + Send + Sync)) -> Result<Option<(Executable, Vec<String>)>, anyhow::Error> {
    let pfx = &bytes[0..2];
    if pfx == [b'#', b'!'] {
        if let Some(eol) = bytes.iter().position(|&x| x == b'\n') {
            let interp = String::from_utf8_lossy(&bytes[2..eol]).trim().to_owned();
            let (exe, mut pre_args) = load_executable_from_filesystem(fs, interp.as_ref(), rt).await?;
            pre_args.insert(0, interp);
            Ok(Some((exe, pre_args)))
        }
        else { Ok(None) }
    }
    else {
        Ok(None)
    }
}
