mod nor;

use serde::{Serialize, de::DeserializeOwned};

use crate::ObjectId;

pub(crate) trait Backend: Sized {
    type Error;
    type Config;

    async fn mount(config: Self::Config) -> Result<Self, Self::Error>;

    async fn write<T: Serialize>(&mut self, id: ObjectId, value: &T) -> Result<(), Self::Error>;

    async fn read<T: DeserializeOwned>(&mut self, id: ObjectId) -> Result<T, Self::Error>;

    async fn remove(&mut self, id: ObjectId) -> Result<(), Self::Error>;

    async fn flush(&mut self) -> Result<(), Self::Error>;

    async fn unmount(mut self) -> Result<(), Self::Error> {
        self.flush().await
    }
}
