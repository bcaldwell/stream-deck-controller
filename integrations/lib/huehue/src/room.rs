use crate::models::generic::GenericIdentifier;
use crate::models::rooms::GetRoomsResponseItem;

pub type Rooms = Vec<Room>;

pub struct Room {
    pub id: uuid::Uuid,
    pub name: String,
    pub services: Vec<GenericIdentifier>,
    pub children: Vec<GenericIdentifier>,
}

impl Room {
    pub fn new(room: GetRoomsResponseItem) -> Room {
        Room {
            id: room.id,
            name: room.metadata.name,
            services: match room.services {
                Some(services) => services,
                None => Vec::new(),
            },
            children: match room.children {
                Some(children) => children,
                None => Vec::new(),
            },
        }
    }
}
