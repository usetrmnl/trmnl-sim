//! Mock central operations traverse the guest's real ATT/GATT host.
#[derive(Clone, Debug, Default)]
pub struct BluetoothStatus {
    pub active: Option<&'static str>,
    pub initialized: bool,
    pub advertising: bool,
    pub connection: Option<u64>,
    pub advertisement: Vec<u8>,
    pub scan_response: Vec<u8>,
}

#[derive(Debug, Clone)]
pub enum BluetoothOperation {
    Connect,
    Disconnect {
        connection: u64,
    },
    Exchange {
        connection: u64,
        data: Vec<u8>,
    },
    /// Retrieve one notification/indication; empty data means no queued update.
    Receive {
        connection: u64,
    },
}

#[derive(Debug, Default)]
pub struct BluetoothReply {
    pub connection: Option<u64>,
    pub data: Vec<u8>,
}
pub type BluetoothReplySender = crossbeam_channel::Sender<Result<BluetoothReply, String>>;
