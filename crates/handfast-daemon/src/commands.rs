use std::sync::Arc;

use async_trait::async_trait;
use handfast_desktop::{CommandSpec, Commands, DesktopError, Result};
use handfast_store::{CommandRecord, Store, StoreError};

const MAX_COMMANDS: usize = 64;
const MAX_KEY_BYTES: usize = 256;
const MAX_NAME_BYTES: usize = 128;
const MAX_ARGV: usize = 16;
const MAX_ARG_BYTES: usize = 1024;

fn valid_text(text: &str, max: usize) -> bool {
    !text.is_empty() && text.len() <= max && !text.chars().any(char::is_control)
}

fn validate(key: &str, name: &str, argv: &[String]) -> std::result::Result<(), DesktopError> {
    if !valid_text(key, MAX_KEY_BYTES) {
        return Err(DesktopError::Backend(
            "the command key is not valid".to_string(),
        ));
    }
    if !valid_text(name, MAX_NAME_BYTES) {
        return Err(DesktopError::Backend(
            "the command name is not valid".to_string(),
        ));
    }
    if argv.is_empty() || argv.len() > MAX_ARGV {
        return Err(DesktopError::Backend(format!(
            "a command needs 1 to {MAX_ARGV} arguments"
        )));
    }
    for argument in argv {
        if argument.len() > MAX_ARG_BYTES || argument.chars().any(char::is_control) {
            return Err(DesktopError::Backend(
                "a command argument is not valid".to_string(),
            ));
        }
    }
    Ok(())
}

fn backend(error: StoreError) -> DesktopError {
    DesktopError::Backend(error.to_string())
}

pub struct StoreCommands {
    store: Arc<Store>,
}

impl StoreCommands {
    #[must_use]
    pub fn new(store: Arc<Store>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Commands for StoreCommands {
    async fn list(&self) -> Result<Vec<CommandSpec>> {
        let records = Arc::clone(&self.store)
            .run_blocking(|store| store.commands())
            .await
            .map_err(backend)?;
        Ok(records
            .into_iter()
            .map(|record| CommandSpec {
                key: record.key,
                name: record.name,
                argv: record.argv,
            })
            .collect())
    }

    async fn get(&self, key: &str) -> Result<Option<CommandSpec>> {
        let key = key.to_string();
        let record = Arc::clone(&self.store)
            .run_blocking(move |store| store.command(&key))
            .await
            .map_err(backend)?;
        Ok(record.map(|record| CommandSpec {
            key: record.key,
            name: record.name,
            argv: record.argv,
        }))
    }

    async fn add(&self, key: &str, name: &str, argv: &[String]) -> Result<()> {
        validate(key, name, argv)?;
        let key = key.to_string();
        let name = name.to_string();
        let argv = argv.to_vec();
        Arc::clone(&self.store)
            .run_blocking(move |store| {
                if store.command(&key)?.is_some() {
                    return Err(StoreError::Data(format!("command {key} already exists")));
                }
                if store.commands()?.len() >= MAX_COMMANDS {
                    return Err(StoreError::Data(format!(
                        "the command list is limited to {MAX_COMMANDS} entries"
                    )));
                }
                store.add_command(&CommandRecord { key, name, argv })
            })
            .await
            .map_err(backend)
    }

    async fn remove(&self, key: &str) -> Result<bool> {
        let key = key.to_string();
        Arc::clone(&self.store)
            .run_blocking(move |store| store.remove_command(&key))
            .await
            .map_err(backend)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adapter() -> (tempfile::TempDir, StoreCommands) {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(directory.path().join("state.db")).unwrap());
        (directory, StoreCommands::new(store))
    }

    fn argv() -> Vec<String> {
        vec!["ls".to_string(), "-l".to_string()]
    }

    #[tokio::test]
    async fn add_list_get_and_remove_round_trip() {
        let (_directory, commands) = adapter();
        commands.add("list", "List files", &argv()).await.unwrap();

        let list = commands.list().await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].key, "list");
        assert_eq!(list[0].name, "List files");
        assert_eq!(list[0].argv, argv());
        assert_eq!(
            commands.get("list").await.unwrap().unwrap().name,
            "List files"
        );

        assert!(commands.remove("list").await.unwrap());
        assert!(!commands.remove("list").await.unwrap());
        assert!(commands.list().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn duplicate_keys_are_rejected() {
        let (_directory, commands) = adapter();
        commands.add("list", "List files", &argv()).await.unwrap();

        let error = commands.add("list", "Another", &argv()).await.unwrap_err();
        assert!(error.to_string().contains("already exists"), "{error}");
    }

    #[tokio::test]
    async fn invalid_commands_are_rejected() {
        let (_directory, commands) = adapter();

        assert!(commands.add("", "Name", &argv()).await.is_err());
        assert!(commands.add("key", "", &argv()).await.is_err());
        assert!(commands.add("key", "Name", &[]).await.is_err());
        assert!(commands
            .add("key", "Name", &["bad\narg".to_string()])
            .await
            .is_err());
        assert!(commands
            .add("key", "Name", &vec!["a".to_string(); MAX_ARGV + 1])
            .await
            .is_err());
    }
}
