use crate::{
    call_settings::CallSettings,
    participant::{LocalParticipant, ParticipantLocation, RemoteParticipant, RemoteVideoTrack},
    IncomingCall,
};
use anyhow::{anyhow, Result};
use audio::{Audio, Sound};
use client::{
    proto::{self, PeerId},
    Client, TypedEnvelope, User, UserStore,
};
use collections::{BTreeMap, HashMap, HashSet};
use fs::Fs;
use futures::{FutureExt, StreamExt};
use gpui::{AppContext, AsyncAppContext, Entity, ModelContext, ModelHandle, Task, WeakModelHandle};
use language::LanguageRegistry;
use live_kit_client::{
    LocalAudioTrack, LocalTrackPublication, LocalVideoTrack, RemoteAudioTrackUpdate,
    RemoteVideoTrackUpdate,
};
use postage::stream::Stream;
use project::Project;
use std::{future::Future, mem, pin::Pin, sync::Arc, time::Duration};
use util::{post_inc, ResultExt, TryFutureExt};

pub const RECONNECT_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    ParticipantLocationChanged {
        participant_id: proto::PeerId,
    },
    RemoteVideoTracksChanged {
        participant_id: proto::PeerId,
    },
    RemoteAudioTracksChanged {
        participant_id: proto::PeerId,
    },
    RemoteProjectShared {
        owner: Arc<User>,
        project_id: u64,
        worktree_root_names: Vec<String>,
    },
    RemoteProjectUnshared {
        project_id: u64,
    },
    Left,
}

pub struct Room {
    id: u64,
    channel_id: Option<u64>,
    live_kit: Option<LiveKitRoom>,
    status: RoomStatus,
    shared_projects: HashSet<WeakModelHandle<Project>>,
    joined_projects: HashSet<WeakModelHandle<Project>>,
    local_participant: LocalParticipant,
    remote_participants: BTreeMap<u64, RemoteParticipant>,
    pending_participants: Vec<Arc<User>>,
    participant_user_ids: HashSet<u64>,
    pending_call_count: usize,
    leave_when_empty: bool,
    client: Arc<Client>,
    user_store: ModelHandle<UserStore>,
    follows_by_leader_id_project_id: HashMap<(PeerId, Option<u64>), Vec<PeerId>>,
    subscriptions: Vec<client::Subscription>,
    pending_room_update: Option<Task<()>>,
    maintain_connection: Option<Task<Option<()>>>,
}

impl Entity for Room {
    type Event = Event;

    fn release(&mut self, cx: &mut AppContext) {
        if self.status.is_online() {
            self.leave_internal(cx).detach_and_log_err(cx);
        }
    }

    fn app_will_quit(&mut self, cx: &mut AppContext) -> Option<Pin<Box<dyn Future<Output = ()>>>> {
        if self.status.is_online() {
            let leave = self.leave_internal(cx);
            Some(
                cx.background()
                    .spawn(async move {
                        leave.await.log_err();
                    })
                    .boxed(),
            )
        } else {
            None
        }
    }
}

impl Room {
    pub fn channel_id(&self) -> Option<u64> {
        self.channel_id
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn is_connected(&self) -> bool {
        if let Some(live_kit) = self.live_kit.as_ref() {
            matches!(
                *live_kit.room.status().borrow(),
                live_kit_client::ConnectionState::Connected { .. }
            )
        } else {
            false
        }
    }

    fn new(
        id: u64,
        channel_id: Option<u64>,
        live_kit_connection_info: Option<proto::LiveKitConnectionInfo>,
        client: Arc<Client>,
        user_store: ModelHandle<UserStore>,
        cx: &mut ModelContext<Self>,
    ) -> Self {
        let live_kit_room = if let Some(connection_info) = live_kit_connection_info {
            let room = live_kit_client::Room::new();
            let mut status = room.status();
            // Consume the initial status of the room.
            let _ = status.try_recv();
            let _maintain_room = cx.spawn_weak(|this, mut cx| async move {
                while let Some(status) = status.next().await {
                    let this = if let Some(this) = this.upgrade(&cx) {
                        this
                    } else {
                        break;
                    };

                    if status == live_kit_client::ConnectionState::Disconnected {
                        this.update(&mut cx, |this, cx| this.leave(cx).log_err());
                        break;
                    }
                }
            });

            let mut track_video_changes = room.remote_video_track_updates();
            let _maintain_video_tracks = cx.spawn_weak(|this, mut cx| async move {
                while let Some(track_change) = track_video_changes.next().await {
                    let this = if let Some(this) = this.upgrade(&cx) {
                        this
                    } else {
                        break;
                    };

                    this.update(&mut cx, |this, cx| {
                        this.remote_video_track_updated(track_change, cx).log_err()
                    });
                }
            });

            let mut track_audio_changes = room.remote_audio_track_updates();
            let _maintain_audio_tracks = cx.spawn_weak(|this, mut cx| async move {
                while let Some(track_change) = track_audio_changes.next().await {
                    let this = if let Some(this) = this.upgrade(&cx) {
                        this
                    } else {
                        break;
                    };

                    this.update(&mut cx, |this, cx| {
                        this.remote_audio_track_updated(track_change, cx).log_err()
                    });
                }
            });

            let connect = room.connect(&connection_info.server_url, &connection_info.token);
            cx.spawn(|this, mut cx| async move {
                connect.await?;

                if !cx.read(Self::mute_on_join) {
                    this.update(&mut cx, |this, cx| this.share_microphone(cx))
                        .await?;
                }

                anyhow::Ok(())
            })
            .detach_and_log_err(cx);

            Some(LiveKitRoom {
                room,
                screen_track: LocalTrack::None,
                microphone_track: LocalTrack::None,
                next_publish_id: 0,
                muted_by_user: false,
                deafened: false,
                speaking: false,
                _maintain_room,
                _maintain_tracks: [_maintain_video_tracks, _maintain_audio_tracks],
            })
        } else {
            None
        };

        let maintain_connection =
            cx.spawn_weak(|this, cx| Self::maintain_connection(this, client.clone(), cx).log_err());

        Audio::play_sound(Sound::Joined, cx);

        Self {
            id,
            channel_id,
            live_kit: live_kit_room,
            status: RoomStatus::Online,
            shared_projects: Default::default(),
            joined_projects: Default::default(),
            participant_user_ids: Default::default(),
            local_participant: Default::default(),
            remote_participants: Default::default(),
            pending_participants: Default::default(),
            pending_call_count: 0,
            subscriptions: vec![client.add_message_handler(cx.handle(), Self::handle_room_updated)],
            leave_when_empty: false,
            pending_room_update: None,
            client,
            user_store,
            follows_by_leader_id_project_id: Default::default(),
            maintain_connection: Some(maintain_connection),
        }
    }

    pub(crate) fn create(
        called_user_id: u64,
        initial_project: Option<ModelHandle<Project>>,
        client: Arc<Client>,
        user_store: ModelHandle<UserStore>,
        cx: &mut AppContext,
    ) -> Task<Result<ModelHandle<Self>>> {
        cx.spawn(|mut cx| async move {
            let response = client.request(proto::CreateRoom {}).await?;
            let room_proto = response.room.ok_or_else(|| anyhow!("invalid room"))?;
            let room = cx.add_model(|cx| {
                Self::new(
                    room_proto.id,
                    None,
                    response.live_kit_connection_info,
                    client,
                    user_store,
                    cx,
                )
            });

            let initial_project_id = if let Some(initial_project) = initial_project {
                let initial_project_id = room
                    .update(&mut cx, |room, cx| {
                        room.share_project(initial_project.clone(), cx)
                    })
                    .await?;
                Some(initial_project_id)
            } else {
                None
            };

            match room
                .update(&mut cx, |room, cx| {
                    room.leave_when_empty = true;
                    room.call(called_user_id, initial_project_id, cx)
                })
                .await
            {
                Ok(()) => Ok(room),
                Err(error) => Err(anyhow!("room creation failed: {:?}", error)),
            }
        })
    }

    pub(crate) fn join_channel(
        channel_id: u64,
        client: Arc<Client>,
        user_store: ModelHandle<UserStore>,
        cx: &mut AppContext,
    ) -> Task<Result<ModelHandle<Self>>> {
        cx.spawn(|cx| async move {
            Self::from_join_response(
                client.request(proto::JoinChannel { channel_id }).await?,
                client,
                user_store,
                cx,
            )
        })
    }

    pub(crate) fn join(
        call: &IncomingCall,
        client: Arc<Client>,
        user_store: ModelHandle<UserStore>,
        cx: &mut AppContext,
    ) -> Task<Result<ModelHandle<Self>>> {
        let id = call.room_id;
        cx.spawn(|cx| async move {
            Self::from_join_response(
                client.request(proto::JoinRoom { id }).await?,
                client,
                user_store,
                cx,
            )
        })
    }

    pub fn mute_on_join(cx: &AppContext) -> bool {
        settings::get::<CallSettings>(cx).mute_on_join || client::IMPERSONATE_LOGIN.is_some()
    }

    fn from_join_response(
        response: proto::JoinRoomResponse,
        client: Arc<Client>,
        user_store: ModelHandle<UserStore>,
        mut cx: AsyncAppContext,
    ) -> Result<ModelHandle<Self>> {
        let room_proto = response.room.ok_or_else(|| anyhow!("invalid room"))?;
        let room = cx.add_model(|cx| {
            Self::new(
                room_proto.id,
                response.channel_id,
                response.live_kit_connection_info,
                client,
                user_store,
                cx,
            )
        });
        room.update(&mut cx, |room, cx| {
            room.leave_when_empty = room.channel_id.is_none();
            room.apply_room_update(room_proto, cx)?;
            anyhow::Ok(())
        })?;
        Ok(room)
    }

    fn should_leave(&self) -> bool {
        self.leave_when_empty
            && self.pending_room_update.is_none()
            && self.pending_participants.is_empty()
            && self.remote_participants.is_empty()
            && self.pending_call_count == 0
    }

    pub(crate) fn leave(&mut self, cx: &mut ModelContext<Self>) -> Task<Result<()>> {
        cx.notify();
        cx.emit(Event::Left);
        self.leave_internal(cx)
    }

    fn leave_internal(&mut self, cx: &mut AppContext) -> Task<Result<()>> {
        if self.status.is_offline() {
            return Task::ready(Err(anyhow!("room is offline")));
        }

        log::info!("leaving room");
        Audio::play_sound(Sound::Leave, cx);

        self.clear_state(cx);

        let leave_room = self.client.request(proto::LeaveRoom {});
        cx.background().spawn(async move {
            leave_room.await?;
            anyhow::Ok(())
        })
    }

    pub(crate) fn clear_state(&mut self, cx: &mut AppContext) {
        for project in self.shared_projects.drain() {
            if let Some(project) = project.upgrade(cx) {
                project.update(cx, |project, cx| {
                    project.unshare(cx).log_err();
                });
            }
        }
        for project in self.joined_projects.drain() {
            if let Some(project) = project.upgrade(cx) {
                project.update(cx, |project, cx| {
                    project.disconnected_from_host(cx);
                    project.close(cx);
                });
            }
        }

        self.status = RoomStatus::Offline;
        self.remote_participants.clear();
        self.pending_participants.clear();
        self.participant_user_ids.clear();
        self.subscriptions.clear();
        self.live_kit.take();
        self.pending_room_update.take();
        self.maintain_connection.take();
    }

    async fn maintain_connection(
        this: WeakModelHandle<Self>,
        client: Arc<Client>,
        mut cx: AsyncAppContext,
    ) -> Result<()> {
        let mut client_status = client.status();
        loop {
            let _ = client_status.try_recv();
            let is_connected = client_status.borrow().is_connected();
            // Even if we're initially connected, any future change of the status means we momentarily disconnected.
            if !is_connected || client_status.next().await.is_some() {
                log::info!("detected client disconnection");

                this.upgrade(&cx)
                    .ok_or_else(|| anyhow!("room was dropped"))?
                    .update(&mut cx, |this, cx| {
                        this.status = RoomStatus::Rejoining;
                        cx.notify();
                    });

                // Wait for client to re-establish a connection to the server.
                {
                    let mut reconnection_timeout = cx.background().timer(RECONNECT_TIMEOUT).fuse();
                    let client_reconnection = async {
                        let mut remaining_attempts = 3;
                        while remaining_attempts > 0 {
                            if client_status.borrow().is_connected() {
                                log::info!("client reconnected, attempting to rejoin room");

                                let Some(this) = this.upgrade(&cx) else { break };
                                if this
                                    .update(&mut cx, |this, cx| this.rejoin(cx))
                                    .await
                                    .log_err()
                                    .is_some()
                                {
                                    return true;
                                } else {
                                    remaining_attempts -= 1;
                                }
                            } else if client_status.borrow().is_signed_out() {
                                return false;
                            }

                            log::info!(
                                "waiting for client status change, remaining attempts {}",
                                remaining_attempts
                            );
                            client_status.next().await;
                        }
                        false
                    }
                    .fuse();
                    futures::pin_mut!(client_reconnection);

                    futures::select_biased! {
                        reconnected = client_reconnection => {
                            if reconnected {
                                log::info!("successfully reconnected to room");
                                // If we successfully joined the room, go back around the loop
                                // waiting for future connection status changes.
                                continue;
                            }
                        }
                        _ = reconnection_timeout => {
                            log::info!("room reconnection timeout expired");
                        }
                    }
                }

                break;
            }
        }

        // The client failed to re-establish a connection to the server
        // or an error occurred while trying to re-join the room. Either way
        // we leave the room and return an error.
        if let Some(this) = this.upgrade(&cx) {
            log::info!("reconnection failed, leaving room");
            let _ = this.update(&mut cx, |this, cx| this.leave(cx));
        }
        Err(anyhow!(
            "can't reconnect to room: client failed to re-establish connection"
        ))
    }

    fn rejoin(&mut self, cx: &mut ModelContext<Self>) -> Task<Result<()>> {
        let mut projects = HashMap::default();
        let mut reshared_projects = Vec::new();
        let mut rejoined_projects = Vec::new();
        self.shared_projects.retain(|project| {
            if let Some(handle) = project.upgrade(cx) {
                let project = handle.read(cx);
                if let Some(project_id) = project.remote_id() {
                    projects.insert(project_id, handle.clone());
                    reshared_projects.push(proto::UpdateProject {
                        project_id,
                        worktrees: project.worktree_metadata_protos(cx),
                    });
                    return true;
                }
            }
            false
        });
        self.joined_projects.retain(|project| {
            if let Some(handle) = project.upgrade(cx) {
                let project = handle.read(cx);
                if let Some(project_id) = project.remote_id() {
                    projects.insert(project_id, handle.clone());
                    rejoined_projects.push(proto::RejoinProject {
                        id: project_id,
                        worktrees: project
                            .worktrees(cx)
                            .map(|worktree| {
                                let worktree = worktree.read(cx);
                                proto::RejoinWorktree {
                                    id: worktree.id().to_proto(),
                                    scan_id: worktree.completed_scan_id() as u64,
                                }
                            })
                            .collect(),
                    });
                }
                return true;
            }
            false
        });

        let response = self.client.request_envelope(proto::RejoinRoom {
            id: self.id,
            reshared_projects,
            rejoined_projects,
        });

        cx.spawn(|this, mut cx| async move {
            let response = response.await?;
            let message_id = response.message_id;
            let response = response.payload;
            let room_proto = response.room.ok_or_else(|| anyhow!("invalid room"))?;
            this.update(&mut cx, |this, cx| {
                this.status = RoomStatus::Online;
                this.apply_room_update(room_proto, cx)?;

                for reshared_project in response.reshared_projects {
                    if let Some(project) = projects.get(&reshared_project.id) {
                        project.update(cx, |project, cx| {
                            project.reshared(reshared_project, cx).log_err();
                        });
                    }
                }

                for rejoined_project in response.rejoined_projects {
                    if let Some(project) = projects.get(&rejoined_project.id) {
                        project.update(cx, |project, cx| {
                            project.rejoined(rejoined_project, message_id, cx).log_err();
                        });
                    }
                }

                anyhow::Ok(())
            })
        })
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn status(&self) -> RoomStatus {
        self.status
    }

    pub fn local_participant(&self) -> &LocalParticipant {
        &self.local_participant
    }

    pub fn remote_participants(&self) -> &BTreeMap<u64, RemoteParticipant> {
        &self.remote_participants
    }

    pub fn remote_participant_for_peer_id(&self, peer_id: PeerId) -> Option<&RemoteParticipant> {
        self.remote_participants
            .values()
            .find(|p| p.peer_id == peer_id)
    }

    pub fn pending_participants(&self) -> &[Arc<User>] {
        &self.pending_participants
    }

    pub fn contains_participant(&self, user_id: u64) -> bool {
        self.participant_user_ids.contains(&user_id)
    }

    pub fn followers_for(&self, leader_id: PeerId, project_id: u64) -> &[PeerId] {
        self.follows_by_leader_id_project_id
            .get(&(leader_id, Some(project_id)))
            .map_or(&[], |v| v.as_slice())
    }

    async fn handle_room_updated(
        this: ModelHandle<Self>,
        envelope: TypedEnvelope<proto::RoomUpdated>,
        _: Arc<Client>,
        mut cx: AsyncAppContext,
    ) -> Result<()> {
        let room = envelope
            .payload
            .room
            .ok_or_else(|| anyhow!("invalid room"))?;
        this.update(&mut cx, |this, cx| this.apply_room_update(room, cx))
    }

    fn apply_room_update(
        &mut self,
        mut room: proto::Room,
        cx: &mut ModelContext<Self>,
    ) -> Result<()> {
        // Filter ourselves out from the room's participants.
        let local_participant_ix = room
            .participants
            .iter()
            .position(|participant| Some(participant.user_id) == self.client.user_id());
        let local_participant = local_participant_ix.map(|ix| room.participants.swap_remove(ix));

        let pending_participant_user_ids = room
            .pending_participants
            .iter()
            .map(|p| p.user_id)
            .collect::<Vec<_>>();

        let remote_participant_user_ids = room
            .participants
            .iter()
            .map(|p| p.user_id)
            .collect::<Vec<_>>();

        let (remote_participants, pending_participants) =
            self.user_store.update(cx, move |user_store, cx| {
                (
                    user_store.get_users(remote_participant_user_ids, cx),
                    user_store.get_users(pending_participant_user_ids, cx),
                )
            });

        self.pending_room_update = Some(cx.spawn(|this, mut cx| async move {
            let (remote_participants, pending_participants) =
                futures::join!(remote_participants, pending_participants);

            this.update(&mut cx, |this, cx| {
                this.participant_user_ids.clear();

                if let Some(participant) = local_participant {
                    this.local_participant.projects = participant.projects;
                } else {
                    this.local_participant.projects.clear();
                }

                if let Some(participants) = remote_participants.log_err() {
                    for (participant, user) in room.participants.into_iter().zip(participants) {
                        let Some(peer_id) = participant.peer_id else {
                            continue;
                        };
                        this.participant_user_ids.insert(participant.user_id);

                        let old_projects = this
                            .remote_participants
                            .get(&participant.user_id)
                            .into_iter()
                            .flat_map(|existing| &existing.projects)
                            .map(|project| project.id)
                            .collect::<HashSet<_>>();
                        let new_projects = participant
                            .projects
                            .iter()
                            .map(|project| project.id)
                            .collect::<HashSet<_>>();

                        for project in &participant.projects {
                            if !old_projects.contains(&project.id) {
                                cx.emit(Event::RemoteProjectShared {
                                    owner: user.clone(),
                                    project_id: project.id,
                                    worktree_root_names: project.worktree_root_names.clone(),
                                });
                            }
                        }

                        for unshared_project_id in old_projects.difference(&new_projects) {
                            this.joined_projects.retain(|project| {
                                if let Some(project) = project.upgrade(cx) {
                                    project.update(cx, |project, cx| {
                                        if project.remote_id() == Some(*unshared_project_id) {
                                            project.disconnected_from_host(cx);
                                            false
                                        } else {
                                            true
                                        }
                                    })
                                } else {
                                    false
                                }
                            });
                            cx.emit(Event::RemoteProjectUnshared {
                                project_id: *unshared_project_id,
                            });
                        }

                        let location = ParticipantLocation::from_proto(participant.location)
                            .unwrap_or(ParticipantLocation::External);
                        if let Some(remote_participant) =
                            this.remote_participants.get_mut(&participant.user_id)
                        {
                            remote_participant.projects = participant.projects;
                            remote_participant.peer_id = peer_id;
                            if location != remote_participant.location {
                                remote_participant.location = location;
                                cx.emit(Event::ParticipantLocationChanged {
                                    participant_id: peer_id,
                                });
                            }
                        } else {
                            this.remote_participants.insert(
                                participant.user_id,
                                RemoteParticipant {
                                    user: user.clone(),
                                    peer_id,
                                    projects: participant.projects,
                                    location,
                                    muted: true,
                                    speaking: false,
                                    video_tracks: Default::default(),
                                    audio_tracks: Default::default(),
                                },
                            );

                            Audio::play_sound(Sound::Joined, cx);

                            if let Some(live_kit) = this.live_kit.as_ref() {
                                let video_tracks =
                                    live_kit.room.remote_video_tracks(&user.id.to_string());
                                let audio_tracks =
                                    live_kit.room.remote_audio_tracks(&user.id.to_string());
                                let publications = live_kit
                                    .room
                                    .remote_audio_track_publications(&user.id.to_string());

                                for track in video_tracks {
                                    this.remote_video_track_updated(
                                        RemoteVideoTrackUpdate::Subscribed(track),
                                        cx,
                                    )
                                    .log_err();
                                }

                                for (track, publication) in
                                    audio_tracks.iter().zip(publications.iter())
                                {
                                    this.remote_audio_track_updated(
                                        RemoteAudioTrackUpdate::Subscribed(
                                            track.clone(),
                                            publication.clone(),
                                        ),
                                        cx,
                                    )
                                    .log_err();
                                }
                            }
                        }
                    }

                    this.remote_participants.retain(|user_id, participant| {
                        if this.participant_user_ids.contains(user_id) {
                            true
                        } else {
                            for project in &participant.projects {
                                cx.emit(Event::RemoteProjectUnshared {
                                    project_id: project.id,
                                });
                            }
                            false
                        }
                    });
                }

                if let Some(pending_participants) = pending_participants.log_err() {
                    this.pending_participants = pending_participants;
                    for participant in &this.pending_participants {
                        this.participant_user_ids.insert(participant.id);
                    }
                }

                this.follows_by_leader_id_project_id.clear();
                for follower in room.followers {
                    let project_id = follower.project_id;
                    let (leader, follower) = match (follower.leader_id, follower.follower_id) {
                        (Some(leader), Some(follower)) => (leader, follower),

                        _ => {
                            log::error!("Follower message {follower:?} missing some state");
                            continue;
                        }
                    };

                    let list = this
                        .follows_by_leader_id_project_id
                        .entry((leader, project_id))
                        .or_insert(Vec::new());
                    if !list.contains(&follower) {
                        list.push(follower);
                    }
                }

                this.pending_room_update.take();
                if this.should_leave() {
                    log::info!("room is empty, leaving");
                    let _ = this.leave(cx);
                }

                this.check_invariants();
                cx.notify();
            });
        }));

        cx.notify();
        Ok(())
    }

    fn remote_video_track_updated(
        &mut self,
        change: RemoteVideoTrackUpdate,
        cx: &mut ModelContext<Self>,
    ) -> Result<()> {
        match change {
            RemoteVideoTrackUpdate::Subscribed(track) => {
                let user_id = track.publisher_id().parse()?;
                let track_id = track.sid().to_string();
                let participant = self
                    .remote_participants
                    .get_mut(&user_id)
                    .ok_or_else(|| anyhow!("subscribed to track by unknown participant"))?;
                participant.video_tracks.insert(
                    track_id.clone(),
                    Arc::new(RemoteVideoTrack {
                        live_kit_track: track,
                    }),
                );
                cx.emit(Event::RemoteVideoTracksChanged {
                    participant_id: participant.peer_id,
                });
            }
            RemoteVideoTrackUpdate::Unsubscribed {
                publisher_id,
                track_id,
            } => {
                let user_id = publisher_id.parse()?;
                let participant = self
                    .remote_participants
                    .get_mut(&user_id)
                    .ok_or_else(|| anyhow!("unsubscribed from track by unknown participant"))?;
                participant.video_tracks.remove(&track_id);
                cx.emit(Event::RemoteVideoTracksChanged {
                    participant_id: participant.peer_id,
                });
            }
        }

        cx.notify();
        Ok(())
    }

    fn remote_audio_track_updated(
        &mut self,
        change: RemoteAudioTrackUpdate,
        cx: &mut ModelContext<Self>,
    ) -> Result<()> {
        match change {
            RemoteAudioTrackUpdate::ActiveSpeakersChanged { speakers } => {
                let mut speaker_ids = speakers
                    .into_iter()
                    .filter_map(|speaker_sid| speaker_sid.parse().ok())
                    .collect::<Vec<u64>>();
                speaker_ids.sort_unstable();
                for (sid, participant) in &mut self.remote_participants {
                    if let Ok(_) = speaker_ids.binary_search(sid) {
                        participant.speaking = true;
                    } else {
                        participant.speaking = false;
                    }
                }
                if let Some(id) = self.client.user_id() {
                    if let Some(room) = &mut self.live_kit {
                        if let Ok(_) = speaker_ids.binary_search(&id) {
                            room.speaking = true;
                        } else {
                            room.speaking = false;
                        }
                    }
                }
                cx.notify();
            }
            RemoteAudioTrackUpdate::MuteChanged { track_id, muted } => {
                let mut found = false;
                for participant in &mut self.remote_participants.values_mut() {
                    for track in participant.audio_tracks.values() {
                        if track.sid() == track_id {
                            found = true;
                            break;
                        }
                    }
                    if found {
                        participant.muted = muted;
                        break;
                    }
                }

                cx.notify();
            }
            RemoteAudioTrackUpdate::Subscribed(track, publication) => {
                let user_id = track.publisher_id().parse()?;
                let track_id = track.sid().to_string();
                let participant = self
                    .remote_participants
                    .get_mut(&user_id)
                    .ok_or_else(|| anyhow!("subscribed to track by unknown participant"))?;

                participant.audio_tracks.insert(track_id.clone(), track);
                participant.muted = publication.is_muted();

                cx.emit(Event::RemoteAudioTracksChanged {
                    participant_id: participant.peer_id,
                });
            }
            RemoteAudioTrackUpdate::Unsubscribed {
                publisher_id,
                track_id,
            } => {
                let user_id = publisher_id.parse()?;
                let participant = self
                    .remote_participants
                    .get_mut(&user_id)
                    .ok_or_else(|| anyhow!("unsubscribed from track by unknown participant"))?;
                participant.audio_tracks.remove(&track_id);
                cx.emit(Event::RemoteAudioTracksChanged {
                    participant_id: participant.peer_id,
                });
            }
        }

        cx.notify();
        Ok(())
    }

    fn check_invariants(&self) {
        #[cfg(any(test, feature = "test-support"))]
        {
            for participant in self.remote_participants.values() {
                assert!(self.participant_user_ids.contains(&participant.user.id));
                assert_ne!(participant.user.id, self.client.user_id().unwrap());
            }

            for participant in &self.pending_participants {
                assert!(self.participant_user_ids.contains(&participant.id));
                assert_ne!(participant.id, self.client.user_id().unwrap());
            }

            assert_eq!(
                self.participant_user_ids.len(),
                self.remote_participants.len() + self.pending_participants.len()
            );
        }
    }

    pub(crate) fn call(
        &mut self,
        called_user_id: u64,
        initial_project_id: Option<u64>,
        cx: &mut ModelContext<Self>,
    ) -> Task<Result<()>> {
        if self.status.is_offline() {
            return Task::ready(Err(anyhow!("room is offline")));
        }

        cx.notify();
        let client = self.client.clone();
        let room_id = self.id;
        self.pending_call_count += 1;
        cx.spawn(|this, mut cx| async move {
            let result = client
                .request(proto::Call {
                    room_id,
                    called_user_id,
                    initial_project_id,
                })
                .await;
            this.update(&mut cx, |this, cx| {
                this.pending_call_count -= 1;
                if this.should_leave() {
                    this.leave(cx).detach_and_log_err(cx);
                }
            });
            result?;
            Ok(())
        })
    }

    pub fn join_project(
        &mut self,
        id: u64,
        language_registry: Arc<LanguageRegistry>,
        fs: Arc<dyn Fs>,
        cx: &mut ModelContext<Self>,
    ) -> Task<Result<ModelHandle<Project>>> {
        let client = self.client.clone();
        let user_store = self.user_store.clone();
        cx.spawn(|this, mut cx| async move {
            let project =
                Project::remote(id, client, user_store, language_registry, fs, cx.clone()).await?;

            this.update(&mut cx, |this, cx| {
                this.joined_projects.retain(|project| {
                    if let Some(project) = project.upgrade(cx) {
                        !project.read(cx).is_read_only()
                    } else {
                        false
                    }
                });
                this.joined_projects.insert(project.downgrade());
            });
            Ok(project)
        })
    }

    pub(crate) fn share_project(
        &mut self,
        project: ModelHandle<Project>,
        cx: &mut ModelContext<Self>,
    ) -> Task<Result<u64>> {
        if let Some(project_id) = project.read(cx).remote_id() {
            return Task::ready(Ok(project_id));
        }

        let request = self.client.request(proto::ShareProject {
            room_id: self.id(),
            worktrees: project.read(cx).worktree_metadata_protos(cx),
        });
        cx.spawn(|this, mut cx| async move {
            let response = request.await?;

            project.update(&mut cx, |project, cx| {
                project.shared(response.project_id, cx)
            })?;

            // If the user's location is in this project, it changes from UnsharedProject to SharedProject.
            this.update(&mut cx, |this, cx| {
                this.shared_projects.insert(project.downgrade());
                let active_project = this.local_participant.active_project.as_ref();
                if active_project.map_or(false, |location| *location == project) {
                    this.set_location(Some(&project), cx)
                } else {
                    Task::ready(Ok(()))
                }
            })
            .await?;

            Ok(response.project_id)
        })
    }

    pub(crate) fn unshare_project(
        &mut self,
        project: ModelHandle<Project>,
        cx: &mut ModelContext<Self>,
    ) -> Result<()> {
        let project_id = match project.read(cx).remote_id() {
            Some(project_id) => project_id,
            None => return Ok(()),
        };

        self.client.send(proto::UnshareProject { project_id })?;
        project.update(cx, |this, cx| this.unshare(cx))
    }

    pub(crate) fn set_location(
        &mut self,
        project: Option<&ModelHandle<Project>>,
        cx: &mut ModelContext<Self>,
    ) -> Task<Result<()>> {
        if self.status.is_offline() {
            return Task::ready(Err(anyhow!("room is offline")));
        }

        let client = self.client.clone();
        let room_id = self.id;
        let location = if let Some(project) = project {
            self.local_participant.active_project = Some(project.downgrade());
            if let Some(project_id) = project.read(cx).remote_id() {
                proto::participant_location::Variant::SharedProject(
                    proto::participant_location::SharedProject { id: project_id },
                )
            } else {
                proto::participant_location::Variant::UnsharedProject(
                    proto::participant_location::UnsharedProject {},
                )
            }
        } else {
            self.local_participant.active_project = None;
            proto::participant_location::Variant::External(proto::participant_location::External {})
        };

        cx.notify();
        cx.foreground().spawn(async move {
            client
                .request(proto::UpdateParticipantLocation {
                    room_id,
                    location: Some(proto::ParticipantLocation {
                        variant: Some(location),
                    }),
                })
                .await?;
            Ok(())
        })
    }

    pub fn is_screen_sharing(&self) -> bool {
        self.live_kit.as_ref().map_or(false, |live_kit| {
            !matches!(live_kit.screen_track, LocalTrack::None)
        })
    }

    pub fn is_sharing_mic(&self) -> bool {
        self.live_kit.as_ref().map_or(false, |live_kit| {
            !matches!(live_kit.microphone_track, LocalTrack::None)
        })
    }

    pub fn is_muted(&self, cx: &AppContext) -> bool {
        self.live_kit
            .as_ref()
            .and_then(|live_kit| match &live_kit.microphone_track {
                LocalTrack::None => Some(Self::mute_on_join(cx)),
                LocalTrack::Pending { muted, .. } => Some(*muted),
                LocalTrack::Published { muted, .. } => Some(*muted),
            })
            .unwrap_or(false)
    }

    pub fn is_speaking(&self) -> bool {
        self.live_kit
            .as_ref()
            .map_or(false, |live_kit| live_kit.speaking)
    }

    pub fn is_deafened(&self) -> Option<bool> {
        self.live_kit.as_ref().map(|live_kit| live_kit.deafened)
    }

    #[track_caller]
    pub fn share_microphone(&mut self, cx: &mut ModelContext<Self>) -> Task<Result<()>> {
        if self.status.is_offline() {
            return Task::ready(Err(anyhow!("room is offline")));
        } else if self.is_sharing_mic() {
            return Task::ready(Err(anyhow!("microphone was already shared")));
        }

        let publish_id = if let Some(live_kit) = self.live_kit.as_mut() {
            let publish_id = post_inc(&mut live_kit.next_publish_id);
            live_kit.microphone_track = LocalTrack::Pending {
                publish_id,
                muted: false,
            };
            cx.notify();
            publish_id
        } else {
            return Task::ready(Err(anyhow!("live-kit was not initialized")));
        };

        cx.spawn_weak(|this, mut cx| async move {
            let publish_track = async {
                let track = LocalAudioTrack::create();
                this.upgrade(&cx)
                    .ok_or_else(|| anyhow!("room was dropped"))?
                    .read_with(&cx, |this, _| {
                        this.live_kit
                            .as_ref()
                            .map(|live_kit| live_kit.room.publish_audio_track(&track))
                    })
                    .ok_or_else(|| anyhow!("live-kit was not initialized"))?
                    .await
            };

            let publication = publish_track.await;
            this.upgrade(&cx)
                .ok_or_else(|| anyhow!("room was dropped"))?
                .update(&mut cx, |this, cx| {
                    let live_kit = this
                        .live_kit
                        .as_mut()
                        .ok_or_else(|| anyhow!("live-kit was not initialized"))?;

                    let (canceled, muted) = if let LocalTrack::Pending {
                        publish_id: cur_publish_id,
                        muted,
                    } = &live_kit.microphone_track
                    {
                        (*cur_publish_id != publish_id, *muted)
                    } else {
                        (true, false)
                    };

                    match publication {
                        Ok(publication) => {
                            if canceled {
                                live_kit.room.unpublish_track(publication);
                            } else {
                                if muted {
                                    cx.background().spawn(publication.set_mute(muted)).detach();
                                }
                                live_kit.microphone_track = LocalTrack::Published {
                                    track_publication: publication,
                                    muted,
                                };
                                cx.notify();
                            }
                            Ok(())
                        }
                        Err(error) => {
                            if canceled {
                                Ok(())
                            } else {
                                live_kit.microphone_track = LocalTrack::None;
                                cx.notify();
                                Err(error)
                            }
                        }
                    }
                })
        })
    }

    pub fn share_screen(&mut self, cx: &mut ModelContext<Self>) -> Task<Result<()>> {
        if self.status.is_offline() {
            return Task::ready(Err(anyhow!("room is offline")));
        } else if self.is_screen_sharing() {
            return Task::ready(Err(anyhow!("screen was already shared")));
        }

        let (displays, publish_id) = if let Some(live_kit) = self.live_kit.as_mut() {
            let publish_id = post_inc(&mut live_kit.next_publish_id);
            live_kit.screen_track = LocalTrack::Pending {
                publish_id,
                muted: false,
            };
            cx.notify();
            (live_kit.room.display_sources(), publish_id)
        } else {
            return Task::ready(Err(anyhow!("live-kit was not initialized")));
        };

        cx.spawn_weak(|this, mut cx| async move {
            let publish_track = async {
                let displays = displays.await?;
                let display = displays
                    .first()
                    .ok_or_else(|| anyhow!("no display found"))?;
                let track = LocalVideoTrack::screen_share_for_display(&display);
                this.upgrade(&cx)
                    .ok_or_else(|| anyhow!("room was dropped"))?
                    .read_with(&cx, |this, _| {
                        this.live_kit
                            .as_ref()
                            .map(|live_kit| live_kit.room.publish_video_track(&track))
                    })
                    .ok_or_else(|| anyhow!("live-kit was not initialized"))?
                    .await
            };

            let publication = publish_track.await;
            this.upgrade(&cx)
                .ok_or_else(|| anyhow!("room was dropped"))?
                .update(&mut cx, |this, cx| {
                    let live_kit = this
                        .live_kit
                        .as_mut()
                        .ok_or_else(|| anyhow!("live-kit was not initialized"))?;

                    let (canceled, muted) = if let LocalTrack::Pending {
                        publish_id: cur_publish_id,
                        muted,
                    } = &live_kit.screen_track
                    {
                        (*cur_publish_id != publish_id, *muted)
                    } else {
                        (true, false)
                    };

                    match publication {
                        Ok(publication) => {
                            if canceled {
                                live_kit.room.unpublish_track(publication);
                            } else {
                                if muted {
                                    cx.background().spawn(publication.set_mute(muted)).detach();
                                }
                                live_kit.screen_track = LocalTrack::Published {
                                    track_publication: publication,
                                    muted,
                                };
                                cx.notify();
                            }

                            Audio::play_sound(Sound::StartScreenshare, cx);

                            Ok(())
                        }
                        Err(error) => {
                            if canceled {
                                Ok(())
                            } else {
                                live_kit.screen_track = LocalTrack::None;
                                cx.notify();
                                Err(error)
                            }
                        }
                    }
                })
        })
    }

    pub fn toggle_mute(&mut self, cx: &mut ModelContext<Self>) -> Result<Task<Result<()>>> {
        let should_mute = !self.is_muted(cx);
        if let Some(live_kit) = self.live_kit.as_mut() {
            if matches!(live_kit.microphone_track, LocalTrack::None) {
                return Ok(self.share_microphone(cx));
            }

            let (ret_task, old_muted) = live_kit.set_mute(should_mute, cx)?;
            live_kit.muted_by_user = should_mute;

            if old_muted == true && live_kit.deafened == true {
                if let Some(task) = self.toggle_deafen(cx).ok() {
                    task.detach();
                }
            }

            Ok(ret_task)
        } else {
            Err(anyhow!("LiveKit not started"))
        }
    }

    pub fn toggle_deafen(&mut self, cx: &mut ModelContext<Self>) -> Result<Task<Result<()>>> {
        if let Some(live_kit) = self.live_kit.as_mut() {
            (*live_kit).deafened = !live_kit.deafened;

            let mut tasks = Vec::with_capacity(self.remote_participants.len());
            // Context notification is sent within set_mute itself.
            let mut mute_task = None;
            // When deafening, mute user's mic as well.
            // When undeafening, unmute user's mic unless it was manually muted prior to deafening.
            if live_kit.deafened || !live_kit.muted_by_user {
                mute_task = Some(live_kit.set_mute(live_kit.deafened, cx)?.0);
            };
            for participant in self.remote_participants.values() {
                for track in live_kit
                    .room
                    .remote_audio_track_publications(&participant.user.id.to_string())
                {
                    tasks.push(cx.foreground().spawn(track.set_enabled(!live_kit.deafened)));
                }
            }

            Ok(cx.foreground().spawn(async move {
                if let Some(mute_task) = mute_task {
                    mute_task.await?;
                }
                for task in tasks {
                    task.await?;
                }
                Ok(())
            }))
        } else {
            Err(anyhow!("LiveKit not started"))
        }
    }

    pub fn unshare_screen(&mut self, cx: &mut ModelContext<Self>) -> Result<()> {
        if self.status.is_offline() {
            return Err(anyhow!("room is offline"));
        }

        let live_kit = self
            .live_kit
            .as_mut()
            .ok_or_else(|| anyhow!("live-kit was not initialized"))?;
        match mem::take(&mut live_kit.screen_track) {
            LocalTrack::None => Err(anyhow!("screen was not shared")),
            LocalTrack::Pending { .. } => {
                cx.notify();
                Ok(())
            }
            LocalTrack::Published {
                track_publication, ..
            } => {
                live_kit.room.unpublish_track(track_publication);
                cx.notify();

                Audio::play_sound(Sound::StopScreenshare, cx);
                Ok(())
            }
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn set_display_sources(&self, sources: Vec<live_kit_client::MacOSDisplay>) {
        self.live_kit
            .as_ref()
            .unwrap()
            .room
            .set_display_sources(sources);
    }
}

struct LiveKitRoom {
    room: Arc<live_kit_client::Room>,
    screen_track: LocalTrack,
    microphone_track: LocalTrack,
    /// Tracks whether we're currently in a muted state due to auto-mute from deafening or manual mute performed by user.
    muted_by_user: bool,
    deafened: bool,
    speaking: bool,
    next_publish_id: usize,
    _maintain_room: Task<()>,
    _maintain_tracks: [Task<()>; 2],
}

impl LiveKitRoom {
    fn set_mute(
        self: &mut LiveKitRoom,
        should_mute: bool,
        cx: &mut ModelContext<Room>,
    ) -> Result<(Task<Result<()>>, bool)> {
        if !should_mute {
            // clear user muting state.
            self.muted_by_user = false;
        }

        let (result, old_muted) = match &mut self.microphone_track {
            LocalTrack::None => Err(anyhow!("microphone was not shared")),
            LocalTrack::Pending { muted, .. } => {
                let old_muted = *muted;
                *muted = should_mute;
                cx.notify();
                Ok((Task::Ready(Some(Ok(()))), old_muted))
            }
            LocalTrack::Published {
                track_publication,
                muted,
            } => {
                let old_muted = *muted;
                *muted = should_mute;
                cx.notify();
                Ok((
                    cx.background().spawn(track_publication.set_mute(*muted)),
                    old_muted,
                ))
            }
        }?;

        if old_muted != should_mute {
            if should_mute {
                Audio::play_sound(Sound::Mute, cx);
            } else {
                Audio::play_sound(Sound::Unmute, cx);
            }
        }

        Ok((result, old_muted))
    }
}

enum LocalTrack {
    None,
    Pending {
        publish_id: usize,
        muted: bool,
    },
    Published {
        track_publication: LocalTrackPublication,
        muted: bool,
    },
}

impl Default for LocalTrack {
    fn default() -> Self {
        Self::None
    }
}

#[derive(Copy, Clone, PartialEq, Eq)]
pub enum RoomStatus {
    Online,
    Rejoining,
    Offline,
}

impl RoomStatus {
    pub fn is_offline(&self) -> bool {
        matches!(self, RoomStatus::Offline)
    }

    pub fn is_online(&self) -> bool {
        matches!(self, RoomStatus::Online)
    }
}
