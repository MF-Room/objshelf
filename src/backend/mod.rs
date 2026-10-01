use serde::{Serialize, de::DeserializeOwned};

use crate::ObjectId;

pub trait Backend: Sized {
    type Error;
    type Config;

    fn mount(config: Self::Config) -> Result<Self, Self::Error>;

    fn write<T: Serialize>(&mut self, id: ObjectId, value: &T) -> Result<(), Self::Error>;

    fn read<T: DeserializeOwned>(&mut self, id: ObjectId) -> Result<T, Self::Error>;

    fn remove(&mut self, id: ObjectId) -> Result<(), Self::Error>;

    fn flush(&mut self) -> Result<(), Self::Error>;

    fn unmount(mut self) -> Result<(), Self::Error> {
        self.flush()
    }
}
