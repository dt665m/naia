use naia_derive::ProtocolType;
use naia_shared::{Manifest, Ref};

mod auth;
mod character;
mod string_message;

pub use auth::Auth;
pub use character::Character;
pub use string_message::StringMessage;

#[derive(ProtocolType, Clone)]
pub enum Protocol {
    Character(Ref<Character>),
    StringMessage(Ref<StringMessage>),
    Auth(Ref<Auth>),
}