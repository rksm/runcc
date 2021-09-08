use std::{
    cmp, io, mem,
    sync::{Arc, Mutex},
};

use tokio::{
    process::{ChildStderr, ChildStdout, Command},
    sync::mpsc,
    task::JoinHandle,
};

use crate::{label::Label, KillBehavior, RunConfig};

use super::command::{CommandInitialized, CommandSpawned, CommandStopped};
use super::kill;

enum CommandState<T> {
    Processing,
    Spawned {
        data: T,
        killer: kill::CommandKiller<T>,
    },
    Stopped(Arc<CommandStopped<T, T>>),
}

#[derive(Clone)]
pub struct CommandSystemKiller<T>(mpsc::Sender<Option<Arc<CommandStopped<T, T>>>>);

impl<T> CommandSystemKiller<T> {
    pub async fn kill_all(&self) {
        let _ = self.0.send(None).await;
    }
}

pub struct CommandSystem<T, P>
where
    P: CommandSystemPlugin<T>,
{
    commands: Arc<Vec<Arc<Mutex<CommandState<T>>>>>,
    killer: CommandSystemKiller<T>,
    handles: Vec<JoinHandle<()>>,
    plugin: Arc<P>,
}

impl<T, P> CommandSystem<T, P>
where
    T: std::marker::Send + std::marker::Sync + 'static,
    P: CommandSystemPlugin<T>,
{
    fn spawn_with_plugin<I>(commands: I, kill_behavior: KillBehavior, plugin: P) -> io::Result<Self>
    where
        I: IntoIterator<Item = (Command, P::CommandInitialData)>,
        P: ,
    {
        let commands: Vec<_> = commands.into_iter().collect();
        let (tx, mut rx) = mpsc::channel(cmp::min(commands.len(), 1));

        let plugin = Arc::new(plugin);

        let (commands, handles): (Vec<_>, Vec<_>) = commands
            .into_iter()
            .map(|(command, data)| {
                let plugin = plugin.clone();
                let (cmd, stdout, stderr) = CommandInitialized::new(command, data).spawn::<T>()?;

                let CommandSpawned {
                    data,
                    join_handle,
                    killer,
                } = cmd;

                let data = plugin.initialize_command_data(data, stdout, stderr);

                let mutex_ret = Arc::new(Mutex::new(CommandState::Spawned { data, killer }));

                let tx = tx.clone();
                let mutex = mutex_ret.clone();

                let handle = tokio::spawn(async move {
                    let cmd = join_handle.join().await;

                    let cmd = {
                        let mut state = mutex.lock().unwrap();

                        let old_state = mem::replace(&mut *state, CommandState::Processing);

                        let cmd = match old_state {
                            CommandState::Spawned { data, killer: _ } => {
                                let cmd = Arc::new(cmd.with_data(data).1);

                                *state = CommandState::Stopped(cmd.clone());

                                if tx.is_closed() {
                                    &plugin.on_command_exited(cmd);
                                    None
                                } else {
                                    &plugin.on_command_exited(cmd.clone());
                                    Some(cmd)
                                }
                            }
                            _ => panic!("unreachable"),
                        };

                        drop(state);
                        cmd
                    };

                    if let Some(cmd) = cmd {
                        let _ = tx.send(Some(cmd)).await;
                    }
                });

                Ok((mutex_ret, handle))
            })
            .collect::<io::Result<Vec<_>>>()?
            .into_iter()
            .unzip();

        let commands_ret = Arc::new(commands);

        let commands = commands_ret.clone();
        let join_killer = tokio::spawn(async move {
            while let Some(exited_cmd) = rx.recv().await {
                let reason = if let Some(ref exited_cmd) = exited_cmd {
                    let should_kill_all: bool = match &kill_behavior {
                        KillBehavior::None => false,
                        KillBehavior::WhenAnyExited => true,
                        KillBehavior::WhenAnyExitedWithStatus(status) => match status {
                            crate::ExitStatusPattern::Success => exited_cmd
                                .exit_status
                                .as_ref()
                                .ok()
                                .map_or(false, |s| s.success()),
                            crate::ExitStatusPattern::Failed => exited_cmd
                                .exit_status
                                .as_ref()
                                .ok()
                                .map_or(true, |s| !s.success()),
                            crate::ExitStatusPattern::StatusCode(code) => exited_cmd
                                .exit_status
                                .as_ref()
                                .ok()
                                .map_or(false, |s| s.code() == Some(*code)),
                        },
                    };

                    if should_kill_all {
                        Some(kill::KillCommandReason::OtherCommandExited(
                            exited_cmd.clone(),
                        ))
                    } else {
                        None
                    }
                } else {
                    // got kill all
                    Some(kill::KillCommandReason::MainProcessGotSignal)
                };

                if let Some(reason) = reason {
                    drop(rx);

                    for state in commands.iter() {
                        let mut state = state.lock().unwrap();

                        match &mut *state {
                            CommandState::Spawned { killer, .. } => {
                                killer.kill(reason.clone());
                            }
                            _ => {}
                        }
                    }

                    break;
                }
            }
        });

        let mut handles = handles;
        handles.push(join_killer);

        Ok(Self {
            commands: commands_ret,
            killer: CommandSystemKiller(tx),
            handles,
            plugin,
        })
    }
}

impl<T: Clone, P: CommandSystemPlugin<T>> CommandSystem<T, P> {
    pub fn share_killer(&self) -> CommandSystemKiller<T> {
        self.killer.clone()
    }
}

impl<T, P: CommandSystemPlugin<T>> CommandSystem<T, P> {
    pub async fn kill_all(&self) {
        self.killer.kill_all().await;
    }

    pub async fn wait_into_stopped_commands(&mut self) -> Vec<Arc<CommandStopped<T, T>>> {
        let Self {
            commands,
            handles,
            plugin,
            ..
        } = self;

        for handle in handles {
            handle.await.expect("CommandSystem subtask panicked");
        }

        let commands = commands
            .iter()
            .map(|cmd| {
                let cmd = cmd.lock().unwrap();

                match &*cmd {
                    CommandState::Stopped(cmd) => cmd.clone(),
                    _ => panic!("CommandState should be stopped after handles joined"),
                }
            })
            .collect();

        if let Some(plugin_join) = plugin.join() {
            let _ = plugin_join.await;
        }

        commands
    }
}

#[derive(Debug, Clone)]
pub struct LabeledCommandData {
    pub label: Label,
}

pub fn spawn_from_run_config_with_plugin<T, P>(
    run_config: RunConfig,
    plugin: P,
) -> io::Result<CommandSystem<T, P>>
where
    T: Send + Sync + 'static,
    P: CommandSystemPlugin<T, CommandInitialData = LabeledCommandData>,
{
    let RunConfig {
        commands,
        max_label_length,
        envs,
        kill,
    } = run_config;

    let commands = commands.into_iter().map(|cmd| {
        let (cmd, label) = cmd.into_tokio_command_and_label(envs.as_ref());

        (
            cmd,
            LabeledCommandData {
                label: Label::from_label(label, max_label_length),
            },
        )
    });

    CommandSystem::spawn_with_plugin(commands, kill, plugin)
}

pub trait CommandSystemPlugin<T>: Send + Sync + 'static + Sized {
    type CommandInitialData;

    fn initialize_command_data(
        &self,
        data: Self::CommandInitialData,
        stdout: ChildStdout,
        stderr: ChildStderr,
    ) -> T;

    fn on_command_exited(&self, _cmd: Arc<CommandStopped<T, T>>) {}

    fn join(&self) -> Option<JoinHandle<()>> {
        None
    }
}