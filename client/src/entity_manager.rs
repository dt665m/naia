use std::{
    collections::{HashMap, VecDeque},
    hash::Hash,
};

use log::warn;

use crate::entity_message_sender::EntityMessageSender;
use naia_shared::{
    DiffMask, EntityActionType, LocalComponentKey, Manifest, NaiaKey, NetEntity, PacketReader,
    PacketWriteState, ProtocolKindType, Protocolize, ReplicateSafe, WorldMutType,
};

use super::{
    entity_action::EntityAction, entity_message_packet_writer::EntityMessagePacketWriter,
    entity_record::EntityRecord,
};

pub struct EntityManager<P: Protocolize, E: Copy + Eq + Hash> {
    entity_records: HashMap<E, EntityRecord<P::Kind>>,
    local_to_world_entity: HashMap<NetEntity, E>,
    component_to_entity_map: HashMap<LocalComponentKey, E>,
    queued_incoming_messages: VecDeque<EntityAction<P, E>>,
    pub entity_message_sender: EntityMessageSender<P, E>,
    message_writer: EntityMessagePacketWriter,
}

impl<P: Protocolize, E: Copy + Eq + Hash> EntityManager<P, E> {
    pub fn new() -> Self {
        EntityManager {
            local_to_world_entity: HashMap::new(),
            entity_records: HashMap::new(),
            component_to_entity_map: HashMap::new(),
            queued_incoming_messages: VecDeque::new(),
            entity_message_sender: EntityMessageSender::new(),
            message_writer: EntityMessagePacketWriter::new(),
        }
    }

    pub fn process_data<W: WorldMutType<P, E>>(
        &mut self,
        world: &mut W,
        manifest: &Manifest<P>,
        server_tick: u16,
        reader: &mut PacketReader,
    ) {
        let entity_action_count = reader.read_u8();

        for _ in 0..entity_action_count {
            let message_type = EntityActionType::from_u8(reader.read_u8());

            match message_type {
                EntityActionType::SpawnEntity => {
                    // Entity Creation
                    let local_id = NetEntity::from_u16(reader.read_u16());
                    let components_num = reader.read_u8();
                    if self.local_to_world_entity.contains_key(&local_id) {
                        // its possible we received a very late duplicate message
                        warn!("attempted to insert duplicate entity");
                        // continue reading, just don't do anything with the data
                        for _ in 0..components_num {
                            let component_kind = P::Kind::from_u16(reader.read_u16());
                            let _component_key = reader.read_u16();
                            manifest.create_replica(component_kind, reader);
                        }
                    } else {
                        // set up entity
                        let world_entity = world.spawn_entity();
                        self.local_to_world_entity.insert(local_id, world_entity);
                        self.entity_records
                            .insert(world_entity, EntityRecord::new(local_id));
                        let entity_record = self.entity_records.get_mut(&world_entity).unwrap();

                        let mut component_list: Vec<P::Kind> = Vec::new();
                        for _ in 0..components_num {
                            // Component Creation //
                            let component_kind = P::Kind::from_u16(reader.read_u16());
                            let component_key = LocalComponentKey::from_u16(reader.read_u16());

                            let new_component = manifest.create_replica(component_kind, reader);
                            if self.component_to_entity_map.contains_key(&component_key) {
                                panic!("attempted to insert duplicate component");
                            } else {
                                {
                                    let new_component_kind = new_component.dyn_ref().kind();
                                    entity_record
                                        .insert_component(&component_key, &new_component_kind);
                                    component_list.push(new_component_kind);
                                }

                                self.component_to_entity_map
                                    .insert(component_key, world_entity);
                                new_component.extract_and_insert(&world_entity, world);
                            }
                            ////////////////////////
                        }

                        self.queued_incoming_messages
                            .push_back(EntityAction::SpawnEntity(world_entity, component_list));
                        continue;
                    }
                }
                EntityActionType::DespawnEntity => {
                    // Entity Deletion
                    let local_id = NetEntity::from_u16(reader.read_u16());
                    if let Some(world_entity) = self.local_to_world_entity.remove(&local_id) {
                        if let Some(entity_record) = self.entity_records.remove(&world_entity) {
                            // Generate event for each component, handing references off just in
                            // case
                            for component_kind in world.component_kinds(&world_entity) {
                                if let Some(component) =
                                    world.remove_component_of_kind(&world_entity, &component_kind)
                                {
                                    self.queued_incoming_messages.push_back(
                                        EntityAction::RemoveComponent(world_entity, component),
                                    );
                                }
                            }

                            for component_key in entity_record.component_keys() {
                                self.component_to_entity_map.remove(&component_key);
                            }

                            world.despawn_entity(&world_entity);

                            self.queued_incoming_messages
                                .push_back(EntityAction::DespawnEntity(world_entity));
                            continue;
                        }
                    }
                    warn!("received message attempting to delete nonexistent entity");
                }
                EntityActionType::MessageEntity => {
                    let net_entity = NetEntity::from_u16(reader.read_u16());
                    let message_kind = P::Kind::from_u16(reader.read_u16());

                    let new_message = manifest.create_replica(message_kind, reader);

                    if !self.local_to_world_entity.contains_key(&net_entity) {
                        // received message BEFORE spawn, or AFTER despawn
                        panic!(
                            "attempting to receive message to nonexistent entity: {}",
                            net_entity.to_u16()
                        );
                    } else {
                        let world_entity = self.local_to_world_entity.get(&net_entity).unwrap();

                        self.queued_incoming_messages
                            .push_back(EntityAction::MessageEntity(*world_entity, new_message));
                    }
                }
                EntityActionType::InsertComponent => {
                    // Add Component to Entity
                    let local_id = NetEntity::from_u16(reader.read_u16());
                    let component_kind = P::Kind::from_u16(reader.read_u16());
                    let component_key = LocalComponentKey::from_u16(reader.read_u16());

                    let new_component = manifest.create_replica(component_kind, reader);
                    if self.component_to_entity_map.contains_key(&component_key) {
                        // its possible we received a very late duplicate message
                        warn!(
                            "attempting to add duplicate local component key: {}, into entity: {}",
                            component_key.to_u16(),
                            local_id.to_u16()
                        );
                    } else {
                        if !self.local_to_world_entity.contains_key(&local_id) {
                            // its possible we received a very late duplicate message
                            warn!(
                                "attempting to add a component: {}, to nonexistent entity: {}",
                                component_key.to_u16(),
                                local_id.to_u16()
                            );
                        } else {
                            let world_entity = self.local_to_world_entity.get(&local_id).unwrap();
                            self.component_to_entity_map
                                .insert(component_key, *world_entity);

                            let entity_record = self.entity_records.get_mut(&world_entity).unwrap();

                            entity_record.insert_component(&component_key, &component_kind);

                            new_component.extract_and_insert(world_entity, world);

                            self.queued_incoming_messages
                                .push_back(EntityAction::InsertComponent(
                                    *world_entity,
                                    component_kind,
                                ));
                        }
                    }
                }
                EntityActionType::UpdateComponent => {
                    // Component Update
                    let component_key = LocalComponentKey::from_u16(reader.read_u16());

                    if let Some(world_entity) = self.component_to_entity_map.get_mut(&component_key)
                    {
                        if let Some(entity_record) = self.entity_records.get(world_entity) {
                            let component_kind =
                                entity_record.kind_from_key(&component_key).unwrap();

                            let diff_mask: DiffMask = DiffMask::read(reader);

                            // read incoming delta
                            world.component_read_partial(
                                world_entity,
                                component_kind,
                                &diff_mask,
                                reader,
                            );

                            self.queued_incoming_messages
                                .push_back(EntityAction::UpdateComponent(
                                    server_tick,
                                    *world_entity,
                                    *component_kind,
                                ));
                        }
                    }
                }
                EntityActionType::RemoveComponent => {
                    // Component Removal
                    let component_key = LocalComponentKey::from_u16(reader.read_u16());

                    if !self.component_to_entity_map.contains_key(&component_key) {
                        // This could happen due to a duplicated unreliable message
                        // (i.e. server re-sends "remove component" message because it believes it
                        // hasn't been delivered and then it does get
                        // delivered after, but then a duplicate message gets delivered too..)
                        warn!(
                            "attempting to remove a non-existent component: {}",
                            component_key.to_u16()
                        );
                    } else {
                        let world_entity =
                            self.component_to_entity_map.remove(&component_key).unwrap();

                        // Get entity record, remove component
                        let component_kind = self
                            .entity_records
                            .get_mut(&world_entity)
                            .expect("entity not instantiated properly? no such entity")
                            .remove_component(&component_key)
                            .expect("entity not instantiated properly? no type");

                        // Get component for last change
                        let component = world
                            .remove_component_of_kind(&world_entity, &component_kind)
                            .expect("Component already removed?");

                        // Generate event
                        self.queued_incoming_messages
                            .push_back(EntityAction::RemoveComponent(world_entity, component));
                    }
                }
                EntityActionType::Unknown => {
                    panic!("received unknown type of entity action");
                }
            }
        }
    }

    pub fn pop_incoming_message(&mut self) -> Option<EntityAction<P, E>> {
        return self.queued_incoming_messages.pop_front();
    }

    // Message Sender
    pub fn send_entity_message<R: ReplicateSafe<P>>(
        &mut self,
        entity: &E,
        message: &R,
        client_tick: u16,
    ) {
        self.entity_message_sender
            .send_entity_message(entity, message, client_tick)
    }

    pub fn has_outgoing_messages(&self) -> bool {
        self.entity_message_sender.has_outgoing_messages()
    }

    // EntityMessagePacketWriter

    pub fn flush_writes(&mut self, out_bytes: &mut Vec<u8>) {
        self.message_writer.flush_writes(out_bytes);
    }

    pub fn queue_writes(&mut self, write_state: &mut PacketWriteState) {
        let mut entity_messages = self.entity_message_sender.generate_outgoing_message_list();

        loop {
            if let Some((_, _, entity, message)) = entity_messages.front() {
                if !self.message_writer.message_fits::<P, E>(
                    write_state,
                    &self.entity_records,
                    &entity,
                    &message,
                ) {
                    break;
                }
            } else {
                break;
            }

            let (message_id, client_tick, entity, message) = entity_messages.pop_front().unwrap();
            self.message_writer.queue_write(
                write_state,
                &self.entity_records,
                &client_tick,
                &entity,
                &message,
            );
            self.entity_message_sender.message_written(
                write_state.packet_index,
                client_tick,
                message_id,
            );
        }
    }
}
