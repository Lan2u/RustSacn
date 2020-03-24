#![warn(missing_docs)]

// Copyright 2020 sacn Developers
//
// Licensed under the Apache License, Version 2.0, <LICENSE-APACHE or
// http://apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. This file may not be
// copied, modified, or distributed except according to those terms.
//
// This file was created as part of a University of St Andrews Computer Science BSC Senior Honours Dissertation Project.

// Report point: There is no guarantees made by the protocol that different sources will have different names.
// As names are used to match universe discovery packets this means that if 2 sources have the same name it won't
// be clear which one is sending what universes as they will appear as one source.

// Report point: partially discovered sources are only marked as discovered when a full set of discovery packets has been
// receieved, if a discovery packet is receieved but there are more pages the source won't be discovered until all the pages are receieved.
// If a page is lost this therefore means the source update / discovery in its entirety will be lost - implementation detail.

/// Socket 2 used for the underlying UDP socket that sACN is sent over.
use socket2::{Domain, Protocol, SockAddr, Socket, Type};

/// Mass import as a very large amount of packet is used here (upwards of 20 items) and this is much cleaner.
use packet::{E131RootLayerData::*, *};

/// Same reasoning as for packet meaning all sacn errors are imported.
use error::errors::{ErrorKind::*, *};

/// The uuid crate is used for working with/generating UUIDs which sACN uses as part of the cid field in the protocol.
/// This is used for uniquely identifying sources when counting sequence numbers.
use uuid::Uuid;

use std::borrow::Cow;
use std::cmp::{max, Ordering};
use std::collections::HashMap;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::{Duration, Instant};

/// Constants required to detect if an IP is IPv4 or IPv6.
#[cfg(target_os = "linux")]
use libc::{AF_INET, AF_INET6};

/// The libc constants required are not avaliable on many windows environments and therefore are hard-coded.
/// Defined as per https://docs.microsoft.com/en-us/windows/win32/api/winsock2/nf-winsock2-socket
#[cfg(target_os = "windows")]
const AF_INET: i32 = 2;

/// Defined as per https://docs.microsoft.com/en-us/windows/win32/api/winsock2/nf-winsock2-socket
#[cfg(target_os = "windows")]
const AF_INET6: i32 = 23;

/// The default size of the buffer used to recieve E1.31 packets.
/// 1143 bytes is biggest packet required as per Section 8 of ANSI E1.31-2018, aligned to 64 bit that is 1144 bytes.
pub const RCV_BUF_DEFAULT_SIZE: usize = 1144;

/// DMX payload size in bytes (512 bytes of data + 1 byte start code).
pub const DMX_PAYLOAD_SIZE: usize = 513;

/// The name of the thread which runs periodically to perform actions on the receiver such as update discovered universes.
pub const RCV_UPDATE_THREAD_NAME: &'static str = "rust_sacn_rcv_update_thread";

/// The default value of the process_preview_data flag.
const PROCESS_PREVIEW_DATA_DEFAULT: bool = false;

/// The default value of the announce_source_discovery flag.
/// Defaults to false based on the assumption that often receivers won't have any immediate response/checks to do on a source
/// announcing itself (every source does this approximately every 10 seconds as per the E131_DISCOVERY_INTERVAL).
const ANNOUNCE_SOURCE_DISCOVERY_DEFAULT: bool = false;

/// The default value of the announce_stream_termination flag.
/// Defaults to false based on the assumption that often receivers will want to ignore termination from a source based on there
/// being multiple possible sources.
const ANNOUNCE_STREAM_TERMINATION_DEFAULT: bool = false;

/// The default value of the announce_timeout flag.
/// 
const ANNOUNCE_TIMEOUT_DEFAULT: bool = false;

/// The sequence number assigned by the receiver to a new source before it has processed the sequence numbers of any data from that source.
/// 
/// This should be set to the value before the initial expected sequence number from a source. Can't do this using underflow as forbidden in Rust.
/// 
const INITIAL_SEQUENCE_NUMBER: u8 = 255;

const DEFAULT_MERGE_FUNC: fn(&DMXData, &DMXData) -> Result<DMXData> =
    discard_lowest_priority_then_previous;

/// Holds a universes worth of DMX data.
#[derive(Debug)]
pub struct DMXData {
    /// The universe that the data was sent to.
    pub universe: u16,
    
    /// The actual universe data, if less than 512 values in length then implies trailing 0's to pad to a full-universe of data.
    pub values: Vec<u8>,

    /// The universe the data is (or was if now acted upon) waiting for a synchronisation packet from.
    /// 0 indicates it isn't waiting for a universe synchronisation packet.
    pub sync_uni: u16,

    /// The priority of the data, this may be useful for receivers which want to implement their own implementing merge algorithms.
    /// Must be less than packet::E131_MAX_PRIORITY.
    pub priority: u8,

    /// The unique id of the source of the data, this may be useful for receivers which want to implement their own merge algorithms
    /// which use the identity of the source to decide behaviour.
    /// A value of None indicates that there is no clear source, for example if a merge algorithm has merged data from 2 or more sources together.
    pub src_cid: Option<Uuid>,

    /// Indicates if the data is marked as 'preview' data indicating it is for use by visualisers etc. as per ANSI E1.31-2018 Section 6.2.6.
    pub preview: bool,
}

/// Used for receiving dmx or other data on a particular universe using multicast.
#[derive(Debug)]
struct SacnNetworkReceiver {
    /// The underlying UDP network socket used.
    socket: Socket,

    /// The address that this SacnNetworkReceiver is bound to.
    addr: SocketAddr,

    /// If true then this receiver supports multicast, is false then it does not.
    /// This flag is set when the receiver is created as not all environments currently support IP multicast.
    /// E.g. IPv6 Windows IP Multicast is currently unsupported.
    is_multicast_enabled: bool,
}

/// Represents an sACN source/sender on the network that has been discovered by this sACN receiver by receiving universe discovery packets.
#[derive(Clone, Debug)]
pub struct DiscoveredSacnSource {
    /// The name of the source, no protocol guarantee this will be unique but if it isn't then universe discovery may not work correctly.
    pub name: String,

    /// The time at which the discovered source was last updated / a discovery packet was received by the source.
    pub last_updated: Instant,

    /// The pages that have been sent so far by this source when enumerating the universes it is currently sending on.   
    pages: Vec<UniversePage>,
    /// The last page that will be sent by this source.
    last_page: u8,
}

/// Universe discovery packets are broken down into pages to allow sending a large list of universes, each page contains a list of universes and
/// which page it is. The receiver then puts the pages together to get the complete list of universes that the discovered source is sending on.
///
/// The concept of pages is intentionally hidden from the end-user of the library as they are a network realisation of what is just an
/// abstract list of universes and don't play any part out-side of the protocol.
#[derive(Eq, Ord, PartialEq, PartialOrd, Clone, Debug)]
struct UniversePage {
    /// The page number of this page.
    page: u8,

    /// The universes that the source is transmitting that are on this page, this may or may-not be a complete list of all universes being sent
    /// depending on if there are more pages.
    universes: Vec<u16>,
}

/// Allows receiving dmx or other (different startcode) data using sacn.
pub struct SacnReceiver {
    /// The SacnNetworkReceiver used for handling communication with UDP / Network / Transport layer.
    receiver: SacnNetworkReceiver,

    /// Data that hasn't been passed up yet as it is waiting e.g. due to universe synchronisation.
    /// Key is the universe. A receiver may not have more than one packet waiting per data_universe.
    /// Data_universe used as key as oppose to sync universe because multiple packets might be waiting on the same sync universe
    /// and adding data by data universe is at least as common as retrieving data by sync address because in a normal setup
    /// 1 or more bits of data wait for 1 sync.
    waiting_data: HashMap<u16, DMXData>,

    /// Universes that this receiver is currently listening for
    universes: Vec<u16>,

    /// Sacn sources that have been discovered by this receiver through universe discovery packets.
    discovered_sources: Vec<DiscoveredSacnSource>,

    /// The merge function used by this receiver if DMXData for the same universe and synchronisation universe is received while there
    /// is already DMXData waiting for that universe and synchronisation address.
    merge_func: fn(&DMXData, &DMXData) -> Result<DMXData>,

    /// Sacn sources that have been partially discovered by only some of their universes being discovered so far with more pages to go.
    partially_discovered_sources: Vec<DiscoveredSacnSource>,

    /// The limit to the number of sources for which to track sequence numbers.
    /// A new source after this limit will cause a SourcesExceededError as per ANSI E1.31-2018 Section 6.2.3.3.
    source_limit: Option<usize>,

    /// The sequence numbers being tracked by this receiver for each packet type, source and universe.
    sequences: SequenceNumbering,

    /// Flag that indicates if this receiver should process packets marked as preview data.
    /// If true then the receiver will process theses packets.
    /// Returned data contains a flag to indicate if it is preview_data which can be used by the implementer to use/discard as required.
    process_preview_data: bool,

    /// Flag which indicates if a SourceDiscovered error should be thrown when receiving data and a source is discovered.
    announce_source_discovery: bool,

    /// Flag which indicates if a StreamTerminated error should be thrown if a receiver receives a stream terminated packet.
    announce_stream_termination: bool,

    /// Flag which indicates if an UniverseTimeout error should be thrown if it is detected that a source has timed out.
    announce_timeout: bool,
}

impl fmt::Debug for SacnReceiver {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}", self.receiver)?;
        write!(f, "{:?}", self.waiting_data)?;
        write!(f, "{:?}", self.universes)?;
        write!(f, "{:?}", self.discovered_sources)?;
        write!(f, "{:?}", self.partially_discovered_sources)
    }
}

impl SacnReceiver {
    /// Creates a new SacnReceiver.
    ///
    /// SacnReceiverInternal is used for actually receiving the sACN data but is wrapped in SacnReceiver to allow the update thread to handle
    /// timeout etc.
    ///
    /// By default for an IPv6 address this will only receieve IPv6 data but IPv4 can also be enabled by calling set_ipv6_only(false).
    /// A receiver with an IPv4 address will only receive IPv4 data.
    ///
    /// IPv6 multicast is unsupported on Windows in Rust. This is due to the underlying library (Socket2) not providing support.
    /// Since UniverseDiscovery is primarily based around multicast to receive the UniverseDiscovery packets this mechanism is expected
    /// to have limited usage when running in an Ipv6 Windows environment. The is_multicast_enabled method can be used to see if multicast
    /// is enabled or not.
    ///
    /// Arguments:
    ///     ip: The address of the interface for this receiver to join, by default this address should use the ACN_SDT_MULTICAST_PORT as defined in
    ///         ANSI E1.31-2018 Appendix A: Defined Parameters (Normative) however another address might be used in some situation.
    ///     source_limit: The limit to the number of sources, past this limit a new source will cause a SourcesExceededError as per ANSI E1.31-2018 Section 6.2.3.3.
    ///                     A source limit of None means no limit to the number of sources.
    ///
    /// # Errors
    /// Will return an InvalidInput error if the source_limit has a value of Some(0) which would indicate this receiver can never receive from any source.
    ///
    /// Will return an error if the SacnReceiver fails to bind to a socket with the given ip.
    /// For more details see socket2::Socket::new().
    ///
    /// Will return an error if the created SacnReceiver fails to listen to the E1.31_DISCOVERY_UNIVERSE.
    /// For more details see SacnReceiver::listen_universes().
    ///
    pub fn with_ip(ip: SocketAddr, source_limit: Option<usize>) -> Result<SacnReceiver> {
        match source_limit {
            Some(x) => {
                if x == 0 {
                    bail!(std::io::Error::new(std::io::ErrorKind::InvalidInput, "Source_limit has a value of Some(0) which would indicate this receiver can never receive from any source"));
                }
            }
            None => {}
        }
        let mut sri = SacnReceiver {
            receiver: SacnNetworkReceiver::new(ip)
                .chain_err(|| "Failed to create SacnNetworkReceiver")?,
            waiting_data: HashMap::new(),
            universes: Vec::new(),
            discovered_sources: Vec::new(),
            merge_func: DEFAULT_MERGE_FUNC,
            partially_discovered_sources: Vec::new(),
            process_preview_data: PROCESS_PREVIEW_DATA_DEFAULT,
            source_limit: source_limit,
            sequences: SequenceNumbering::new(),
            announce_source_discovery: ANNOUNCE_SOURCE_DISCOVERY_DEFAULT,
            announce_stream_termination: ANNOUNCE_STREAM_TERMINATION_DEFAULT,
            announce_timeout: ANNOUNCE_TIMEOUT_DEFAULT,
        };

        sri.listen_universes(&[E131_DISCOVERY_UNIVERSE])
            .chain_err(|| "Failed to listen to discovery universe")?;

        Ok(sri)
    }

    /// Sets the value of the is_multicast_enabled flag to the given value.
    ///
    /// If set to false then the receiver won't attempt to join any more multicast groups.
    ///
    /// This method does not attempt to leave multicast groups already joined through previous listen_universe calls.
    ///
    /// # Arguments
    /// val: The new value for the is_multicast_enabled flag.
    ///
    /// # Errors
    /// Will return an OsOperationUnsupported error if attempting to set the flag to true in an environment that multicast
    /// isn't supported i.e. Ipv6 on Windows.
    pub fn set_is_multicast_enabled(&mut self, val: bool) -> Result<()> {
        self.receiver.set_is_multicast_enabled(val)
    }

    /// Returns true if multicast is enabled on this receiver and false if not.
    /// This flag is set when the receiver is created as not all environments currently support IP multicast.
    /// E.g. IPv6 Windows IP Multicast is currently unsupported.
    pub fn is_multicast_enabled(&self) -> bool {
        self.receiver.is_multicast_enabled()
    }

    /// Wipes the record of discovered and sequence number tracked sources.
    /// This is one way to handle a sources exceeded condition.
    pub fn reset_sources(&mut self) {
        self.sequences.clear();
        self.partially_discovered_sources.clear();
        self.discovered_sources.clear();
    }

    /// Sets the merge function to be used by this receiver.
    ///
    /// This merge function is called if data is waiting for a universe e.g. for syncronisation and then further data for that universe with the same
    /// syncronisation address arrives.
    ///
    /// This merge function MUST return a DmxMergeError if there is a problem merging. This error can optionally encapsulate further errors using the Error-chain system
    ///     to provide a more informative backtrace.
    ///
    /// Arguments:
    /// func: The merge function to use. Should take 2 DMXData references as arguments and return a Result<DMXData>.
    pub fn set_merge_fn(&mut self, func: fn(&DMXData, &DMXData) -> Result<DMXData>) -> Result<()> {
        self.merge_func = func;
        Ok(())
    }

    /// Allow only receiving on Ipv6.
    pub fn set_ipv6_only(&mut self, val: bool) -> Result<()> {
        self.receiver.set_only_v6(val)
    }

    /// Deletes all data currently waiting to be passed up - e.g. waiting for a synchronisation packet.
    pub fn clear_all_waiting_data(&mut self) {
        self.waiting_data.clear();
    }

    /// Clears data (if any) waiting to be passed up for the specific universe.
    /// 
    /// # Arguments
    /// 
    /// universe: The universe that the data that is waiting was sent to.
    /// 
    pub fn clear_waiting_data(&mut self, universe: u16) {
        self.waiting_data.remove(&universe);
    }

    /// Allows receiving from the given universe and starts listening to the multicast addresses which corresponds to the given universe.
    ///
    /// Note that if the is_multicast_enabled flag is set to false then this method will only register the universe to listen to and won't
    /// attempt to join any multicast groups.
    ///
    /// If 1 or more universes in the list are already being listened to this method will have no effect for those universes only.
    ///
    /// # Errors
    /// Returns an ErrorKind::IllegalUniverse error if the given universe is outwith the allowed range of universes,
    /// see (is_universe_in_range)[fn.is_universe_in_range.packet].
    ///
    /// Will also return an Error if there is an issue listening to the multicast universe, see SacnNetworkReceiver::listen_multicast_universe().
    ///
    pub fn listen_universes(&mut self, universes: &[u16]) -> Result<()> {
        for u in universes {
            is_universe_in_range(*u)?;
        }

        for u in universes {
            match self.universes.binary_search(u) {
                Err(i) => {
                    // Value not found, i is the position it should be inserted
                    self.universes.insert(i, *u);

                    if self.is_multicast_enabled() {
                        self.receiver
                            .listen_multicast_universe(*u)
                            .chain_err(|| "Failed to listen to multicast universe")?;
                    }
                }
                Ok(_) => { // If value found then don't insert to avoid duplicates.
                }
            }
        }

        Ok(())
    }
    /// Stops listening to the given universe.
    ///
    /// # Errors
    /// Returns an ErrorKind::IllegalUniverse error if the given universe is outwith the allowed range of universes,
    /// see (is_universe_in_range)[fn.is_universe_in_range.packet].
    ///
    /// Returns UniverseNotFound if the given universe wasn't already being listened to.
    ///
    pub fn mute_universe(&mut self, universe: u16) -> Result<()> {
        is_universe_in_range(universe)?;

        match self.universes.binary_search(&universe) {
            Err(_) => {
                // Universe isn't found.
                bail!(ErrorKind::UniverseNotFound(
                    "Attempted to mute a universe that wasn't already being listened to"
                        .to_string()
                ));
            }
            Ok(i) => {
                // If value found then don't insert to avoid duplicates.
                self.universes.remove(i);
                self.receiver.mute_multicast_universe(universe)
            }
        }
    }

    /// Set the process_preview_data flag to the given value.
    ///
    /// This flag indicates if this receiver should process packets marked as preview_data or should ignore them.
    ///
    /// Argument:
    /// val: The new value of process_preview_data flag.
    pub fn set_process_preview_data(&mut self, val: bool) {
        self.process_preview_data = val;
    }

    /// Removes the given universe from the discovered sACN source with the given name, also stops tracking 
    /// sequence numbers for that universe / sender combination.
    ///
    /// Note this is just a record keeping operation, it doesn't actually effect the real sACN sender it
    /// just updates the record of what universes are expected on this receiver.
    /// 
    /// If the src_cid/source_name/univese isn't currently registered then this method has no effect.
    /// This is intentional as it allows calling this function multiple times without worrying about failure because 
    /// it comes to the same result.
    ///     E.g. when a source terminates it sends 3 termination packets but a receiver should only terminate once.
    ///
    /// # Arguments:
    /// 
    /// src_cid: The CID of the source which is terminating a universe.
    /// 
    /// source_name: The human readable name of the sACN source to remove the universe from.
    /// 
    /// universe:    The sACN universe to remove.
    /// 
    fn terminate_stream<'a>(&mut self, src_cid: Uuid, source_name: Cow<'a, str>, universe: u16) {
        match self.sequences.remove_seq_numbers(src_cid, universe) {
            _ => {
                // Will only return an error if the source/universe wasn't found which is acceptable because as it
                // comes to the same result.
            } 
        }

        match find_discovered_src(&self.discovered_sources, &source_name.to_string()) {
            Some(index) => {
                self.discovered_sources[index].terminate_universe(universe);
            }
            None => { 
                // As with sequence numbers the source might not be found which is acceptable.
            }
        }
    }

    /// Checks if this receiver is currently listening to the given universe.
    /// 
    /// A receiver is 'listening' to a universe if it allows that universe to be received without filtering it out.
    /// This does not mean that the multicast address for that universe is or isn't being listened to.
    /// 
    /// Arguments:
    /// universe: The sACN universe to check
    /// 
    /// Returns:
    /// True if the universe is being listened to by this receiver, false if not.
    pub fn is_listening(&self, universe: &u16) -> bool {
        self.universes.contains(universe)
    }

    /// Handles the given data packet for this DMX reciever.
    ///
    /// Returns the universe data if successful.
    /// If the returned value is None it indicates that the data was received successfully but isn't ready to act on.
    ///
    /// Synchronised data packets handled as per ANSI E1.31-2018 Section 6.2.4.1.
    ///
    /// Arguments:
    /// data_pkt: The sACN data packet to handle.
    ///
    /// # Errors
    /// Returns an OutOfSequence error if a packet is received out of order as detected by the different between
    /// the packets sequence number and the expected sequence number as specified in ANSI E1.31-2018 Section 6.7.2 Sequence Numbering.
    ///
    /// Returns a UniversesTerminated error if a packet is received with the stream_terminated flag set indicating that the source is no longer
    /// sending on that universe and the announce_stream_termination_flag is set to true.
    ///
    /// Will return an DmxMergeError if there is an issue merging or replacing new and existing waiting data.
    ///
    fn handle_data_packet(
        &mut self,
        cid: Uuid,
        data_pkt: DataPacketFramingLayer,
    ) -> Result<Option<Vec<DMXData>>> {

        if data_pkt.preview_data && !self.process_preview_data {
            // Don't process preview data unless receiver has process_preview_data flag set.
            return Ok(None);
        }

        if data_pkt.stream_terminated {
            self.terminate_stream(cid, data_pkt.source_name, data_pkt.universe);
            if self.announce_stream_termination {
                bail!(ErrorKind::UniverseTerminated(
                    "A source terminated a universe and this was detected when trying to receive data"
                        .to_string()
                ));
            }
            
            return Ok(None);
        }

        if !self.is_listening(&data_pkt.universe) {
            return Ok(None); // If not listening for this universe then ignore the packet.
        }

        // Preview data and stream terminated both get precedence over checking the sequence number.
        // This is as per ANSI E1.31-2018 Section 6.2.6, Stream_Terminated: Bit 6, 'Any property values
        // in an E1.31 Data Packet containing this bit shall be ignored'

        self.sequences.check_data_seq_number(
            self.source_limit,
            cid,
            data_pkt.sequence_number,
            data_pkt.universe,
            self.announce_timeout
        )?;
        if data_pkt.synchronization_address == E131_NO_SYNC_ADDR {
            self.clear_waiting_data(data_pkt.universe);

            let vals: Vec<u8> = data_pkt.data.property_values.into_owned();
            let dmx_data: DMXData = DMXData {
                universe: data_pkt.universe,
                values: vals.to_vec(),
                sync_uni: data_pkt.synchronization_address,
                priority: data_pkt.priority,
                src_cid: Some(cid),
                preview: data_pkt.preview_data
            };

            return Ok(Some(vec![dmx_data]));
        } else {
            let vals: Vec<u8> = data_pkt.data.property_values.into_owned();
            let dmx_data: DMXData = DMXData {
                universe: data_pkt.universe,
                values: vals.to_vec(),
                sync_uni: data_pkt.synchronization_address,
                priority: data_pkt.priority,
                src_cid: Some(cid),
                preview: data_pkt.preview_data
            };

            self.store_waiting_data(dmx_data)?;
            Ok(None)
        }
    }

    /// Takes the given data and trys to add it to the waiting data.
    /// 
    /// Note that a receiver will only store a single packet of data per data_universe at once.
    /// 
    /// If there is waiting data for the same universe as the data then it will be merged as per the
    /// merge_func which by default keeps the highest priority data, if the data has the same priority
    /// then the newest data is kept.
    ///
    /// # Errors
    /// Will return an DmxMergeError if there is an issue merging or replacing new and existing waiting data.
    ///
    fn store_waiting_data(&mut self, data: DMXData) -> Result<()> {
        
        match self.waiting_data.remove(&data.universe) {
            Some(existing) => {
                self.waiting_data.insert(data.universe, ((self.merge_func)(&existing, &data))?);
            }
            None => {
                self.waiting_data.insert(data.universe, data);
            }
        }
        Ok(())
    }

    /// Handles the given synchronisation packet for this DMX receiver.
    ///
    /// Synchronisation packets handled as described by ANSI E1.31-2018 Section 6.2.4.1.
    ///
    /// Returns the released / previously blocked data if successful.
    /// If the returned Vec is empty it indicates that no data was waiting.
    ///
    /// E1.31 Synchronization Packets occur on specific universes. Upon receipt, they indicate that any data advertising that universe as its Synchronization Address must be acted upon.
    /// In an E1.31 Data Packet, a value of 0 in the Synchronization Address indicates that the universe data is not synchronized. If a receiver is presented with an E1.31 Data Packet
    /// containing a Synchronization Address of 0, it shall discard any data waiting to be processed and immediately act on that Data Packet.
    ///
    /// If the Synchronization Address field is not 0, and the receiver is receiving an active synchronization stream for that Synchronization Address,
    /// it shall hold that E1.31 Data Packet until the arrival of the appropriate E1.31 Synchronization Packet before acting on it.
    ///
    /// Arguments:
    /// sync_pkt: The E1.31 synchronisation part of the synchronisation packet to handle.
    ///
    /// # Errors
    /// Returns an OutOfSequence error if a packet is received out of order as detected by the different between
    /// the packets sequence number and the expected sequence number as specified in ANSI E1.31-2018 Section 6.7.2 Sequence Numbering.
    ///
    fn handle_sync_packet(
        &mut self,
        cid: Uuid,
        sync_pkt: SynchronizationPacketFramingLayer,
    ) -> Result<Option<Vec<DMXData>>> {
        if !self.is_listening(&sync_pkt.synchronization_address) {
            return Ok(None); // If not listening for this universe then ignore the packet.
        }

        self.sequences.check_sync_seq_number(
            self.source_limit,
            cid,
            sync_pkt.sequence_number,
            sync_pkt.synchronization_address,
            self.announce_timeout,
        )?;

        let res = self.rtrv_waiting_data(sync_pkt.synchronization_address);
        if res.len() == 0 {
            Ok(None)
        } else {
            Ok(Some(res))
        }
    }

    /// Retrieves and removes the DMX data of all waiting data with a synchronisation address matching the one provided.
    /// Returns an empty Vec if there is no data waiting.
    ///
    /// Arguments:
    /// sync_uni: The synchronisation universe of the data that should be retrieved.
    /// 
    fn rtrv_waiting_data(&mut self, sync_uni: u16) -> Vec<DMXData> {
        // Get the universes (used as keys) to remove and then move the corresponding data out of the waiting data and into the result.
        // This prevents having to copy DMXData.
        // Cannot do both actions at once as cannot modify a data structure while iterating over it.
        let mut keys: Vec<u16> = Vec::new();
        for (uni, data) in self.waiting_data.iter() {
            if data.sync_uni == sync_uni {
                keys.push(*uni);
            }
        }

        let mut res: Vec<DMXData> = Vec::new();
        for k in keys {
            res.push(self.waiting_data.remove(&k).unwrap());
        }

        res
    }

    /// Takes the given DiscoveredSacnSource and updates the record of discovered sacn sources.
    ///
    /// This adds the new source deleting any previous source with the same name.
    ///
    /// Arguments:
    /// src: The DiscoveredSacnSource to update the record of discovered sacn sources with.
    fn update_discovered_srcs(&mut self, src: DiscoveredSacnSource) {
        match find_discovered_src(&self.discovered_sources, &src.name) {
            Some(index) => {
                self.discovered_sources.remove(index);
            }
            None => {}
        }
        self.discovered_sources.push(src);
    }

    /// Handles the given universe discovery packet.
    ///
    /// This universe discovery packet might be the whole thing or may be just one page of a discovery packet.
    /// This method puts the pages to produce the DiscoveredSacnSource which is stored in the receiver.
    ///
    /// Returns true if a source was fully discovered or false if the source was only partially discovered.
    ///
    /// Arguments:
    /// discovery_pkt: The universe discovery part of the universe discovery packet to handle.
    ///
    fn handle_universe_discovery_packet(
        &mut self,
        discovery_pkt: UniverseDiscoveryPacketFramingLayer,
    ) -> bool {
        let data: UniverseDiscoveryPacketUniverseDiscoveryLayer = discovery_pkt.data;

        let page: u8 = data.page;
        let last_page: u8 = data.last_page;

        let universes = data.universes;

        let uni_page: UniversePage = UniversePage {
            page: page,
            universes: universes.into(),
        };

        // See if some pages that belong to the source that this page belongs to have already been received.
        match find_discovered_src(
            &self.partially_discovered_sources,
            &discovery_pkt.source_name.to_string(),
        ) {
            Some(index) => {
                // Some pages have already been received from this source.
                self.partially_discovered_sources[index]
                    .pages
                    .push(uni_page);
                self.partially_discovered_sources[index].last_updated = Instant::now();
                if self.partially_discovered_sources[index].has_all_pages() {
                    let discovered_src: DiscoveredSacnSource =
                        self.partially_discovered_sources.remove(index);
                    self.update_discovered_srcs(discovered_src);
                    return true;
                }
            }
            None => {
                // This is the first page received from this source.
                let discovered_src: DiscoveredSacnSource = DiscoveredSacnSource {
                    name: discovery_pkt.source_name.to_string(),
                    last_page: last_page,
                    pages: vec![uni_page],
                    last_updated: Instant::now(),
                };

                if page == 0 && page == last_page {
                    // Indicates that this is a single page universe discovery packet.
                    self.update_discovered_srcs(discovered_src);
                    return true;
                } else {
                    // Indicates that this is a page in a set of pages as part of a sources universe discovery.
                    self.partially_discovered_sources.push(discovered_src);
                }
            }
        }

        return false;
    }

    /// Attempt to recieve data from any of the registered universes.
    /// This is the main method for receiving data.
    /// Any data returned will be ready to act on immediately i.e. waiting e.g. for universe synchronisation
    /// is already handled.
    ///
    /// # Errors
    /// This method will return a WouldBlock (unix) or TimedOut (windows) error if there is no data ready within the given timeout.
    /// A timeout of duration 0 will instantly return a WouldBlock/TimedOut error without checking for data.
    ///
    /// Will return ErrorKind::SourceDiscovered error if the announce_source_discovery flag is set and a universe discovery
    /// packet is received and a source fully discovered.
    ///
    /// Will return a UniverseNotRegistered error if this method is called with an infinite timeout, no
    /// registered data universes and the announce_discovered_sources flag set to off. This is to protect the user from
    /// making this mistake leading to the method never being able to return.
    ///
    /// The method may also return an error if there is an issue setting a timeout on the receiver. See
    /// SacnNetworkReceiver::set_timeout for details.
    ///
    /// The method may also return an error if there is an issue handling the data as either a Data, Syncronisation or Discovery packet.
    /// See the SacnReceiver::handle_data_packet, SacnReceiver::handle_sync_packet and SacnReceiver::handle_universe_discovery_packet methods
    /// for details.
    pub fn recv(&mut self, timeout: Option<Duration>) -> Result<Vec<DMXData>> {
        if self.universes.len() == 1
            && self.universes[0] == E131_DISCOVERY_UNIVERSE
            && timeout.is_none()
            && !self.announce_source_discovery
        {
            // This indicates that the only universe that can be received is the discovery universe.
            // This means that having no timeout may lead to no data ever being received and so this method blocking forever
            // to prevent this likely unintended behaviour throw a universe not registered error.
            bail!(ErrorKind::UniverseNotRegistered("Attempting to receive data with no data universes registered, an infinite timeout and no discovery announcements".to_string()));
        }

        let mut buf: [u8; RCV_BUF_DEFAULT_SIZE] = [0; RCV_BUF_DEFAULT_SIZE];

        if timeout == Some(Duration::from_secs(0)) {
            if cfg!(target_os = "windows") { // Use the right expected error for the operating system.
                bail!(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "No data avaliable in given timeout"
                ));
            } else {
                bail!(std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    "No data avaliable in given timeout"
                ));
            }
        }

        self.receiver
            .set_timeout(timeout)
            .chain_err(|| "Failed to sent a timeout value for the receiver")?;
        let start_time = Instant::now();

        match self.receiver.recv(&mut buf) {
            Ok(pkt) => {
                let pdu: E131RootLayer = pkt.pdu;
                let data: E131RootLayerData = pdu.data;
                let res = match data {
                    DataPacket(d) => self
                        .handle_data_packet(pdu.cid, d)?,
                    SynchronizationPacket(s) => self
                        .handle_sync_packet(pdu.cid, s)?,
                    UniverseDiscoveryPacket(u) => {
                        if self.handle_universe_discovery_packet(u)
                            && self.announce_source_discovery
                        {
                            bail!(ErrorKind::SourceDiscovered(
                                "Receiver discovered a source".to_string()
                            ));
                        } else {
                            None
                        }
                    }
                };
                match res {
                    Some(r) => Ok(r),
                    None => {
                        // Indicates that there is no data ready to pass up yet even if a packet was received.
                        // To stop recv blocking forever with a non-None timeout due to packets being received consistently (that reset the timeout)
                        // within the receive timeout (e.g. universe discovery packets if the discovery interval < timeout) the timeout needs to be
                        // adjusted to account for the time already taken.
                        if !timeout.is_none() {
                            let elapsed = start_time.elapsed();
                            match timeout.unwrap().checked_sub(elapsed) {
                                None => {
                                    // Indicates that elapsed is bigger than timeout so its time to return.
                                    bail!(std::io::Error::new(
                                        std::io::ErrorKind::WouldBlock,
                                        "No data avaliable in given timeout"
                                    ));
                                }
                                Some(new_timeout) => return self.recv(Some(new_timeout)),
                            }
                        } else {
                            // If the timeout was none then would keep looping till data is returned as the method should keep blocking till then.
                            self.recv(timeout)
                        }
                    }
                }
            }
            Err(err) => Err(err),
        }
    }

    /// Returns a list of the sources that have been discovered on the network by this receiver through the E1.31 universe discovery mechanism.
    pub fn get_discovered_sources(&mut self) -> Vec<DiscoveredSacnSource> {
        self.remove_expired_sources();
        self.discovered_sources.clone()
    }

    /// Gets all discovered sources without checking if any are timed out.
    /// As the sources may be timed out get_discovered_sources is the preferred method but this is included
    /// to allow receivers to disable source timeouts which may be useful in very high latency networks.
    pub fn get_discovered_sources_no_check(&mut self) -> Vec<DiscoveredSacnSource> {
        self.discovered_sources.clone()
    }

    /// Goes through all discovered sources and removes any that have timed out
    fn remove_expired_sources(&mut self) {
        self.partially_discovered_sources
            .retain(|s| s.last_updated.elapsed() < UNIVERSE_DISCOVERY_SOURCE_TIMEOUT);
        self.discovered_sources
            .retain(|s| s.last_updated.elapsed() < UNIVERSE_DISCOVERY_SOURCE_TIMEOUT);
    }
    /// Returns the current value of the announce_source_discovery flag.
    /// See (set_announce_source_discovery)[receive::set_announce_source_discovery] for an explanation of the flag.
    pub fn get_announce_source_discovery(&self) -> bool {
        self.announce_source_discovery
    }

    /// Sets the value of the announce_source_discovery flag to the given value.
    ///
    /// By default this flag is false which indicates that when receiving data discovered sources through universe discovery
    ///  won't be announced by the recv method and the receivers list of discovered universes will be updated silently.
    /// If set to true then it means that a SourceDiscovered error will be thrown whenever a source is discovered through a
    ///  complete universe discovery packet.
    ///
    /// # Arguments:
    /// new_val: The new value for the announce_source_discovery flag.
    ///
    pub fn set_announce_source_discovery(&mut self, new_val: bool) {
        self.announce_source_discovery = new_val;
    }

    /// Returns the current value of the announce_timeout flag.
    /// See (set_announce_timeout)[set_announce_timeout] for an explanation of the flag.
    pub fn get_announce_timeout(&self) -> bool {
        self.announce_timeout
    }

    /// Sets the value of the announce_timeout flag to the given value.
    /// 
    /// By default this flag is false which means that if a universe for a source times out due to data not being sent then
    /// this will be updated on the receiver silently.
    /// If set to true then a UniverseTimeout error will be thrown when attempting to receive if it is detected that a source universe has 
    /// timed out as per ANSI E1.31-2018 Section 6.7.1.
    /// 
    /// # Arguments:
    /// new_val: The new value for the announce_timeout flag.
    /// 
    pub fn set_announce_timeout(&mut self, new_val: bool) {
        self.announce_timeout = new_val;
    }

    /// Returns the current value of the announce_stream_termination flag.
    /// See (set_announce_stream_termination)[set_announce_stream_termination] for an explanation of the flag.
    pub fn get_announce_stream_termination(&self) -> bool {
        self.announce_stream_termination
    }

    /// Sets the value of the announce_stream_termination flag to the given value.
    /// 
    /// By default this flag is false. This indicates that if a source sends a stream termination packet it will be handled silently by the receiver.
    /// If set to true then a UniverseTermination error will be thrown when attempting to receive if a termination packet is received as per
    /// ANSI E1.31-2018 Section 6.2.6.
    /// 
    pub fn set_announce_stream_termination(&mut self, new_val: bool) {
        self.announce_stream_termination = new_val;
    }
}

/// By implementing the Drop trait for SacnNetworkReceiver it means that the user doesn't have to explicitly cleanup the receiver
/// and if it goes out of reference it will clean itself up.
impl Drop for SacnReceiver {
    fn drop(&mut self){
        let universes = self.universes.clone();
        for u in universes {
            // Cannot return an error or pass it onto the user because drop might be called during a panic.
            // Therefore if there is an error cleaning up the only options are ignore, notify or panic.
            // Notify using stdout might pollute the application using the library so would require a flag to enable/disable but the function of this
            // is unclear and the problem isn't solved if the flag is disabled.
            // A panic might be unnessesary or pollute another in-progress panic hiding the true problem. It would also prevent muting the other
            // universes.
            // The error is therefore ignored as it can't be fixed eitherway as the SacnReceiver has gone out of scope and won't lead to memory unsafety.
            match self.mute_universe(u) {
                Ok(_) => {}
                Err(_e) => {/* Ignored */}
            }
        }
    }
}


/// Searches for the discovered source with the given name in the given vector of discovered sources and
/// returns the index of the src in the Vec or None if not found.
///
/// Arguments:
/// srcs: The Vec of DiscoveredSacnSources to search.
/// name: The human readable name of the source to find.
///
fn find_discovered_src(srcs: &Vec<DiscoveredSacnSource>, name: &String) -> Option<usize> {
    for i in 0..srcs.len() {
        if srcs[i].name == *name {
            return Some(i);
        }
    }
    None
}

/// In general the lower level transport layer is handled by SacnNetworkReceiver (which itself wraps a Socket).
/// Windows and linux handle multicast sockets differently.
/// This is built for / tested with Windows 10 1909.
#[cfg(target_os = "windows")]
impl SacnNetworkReceiver {
    /// Creates a new DMX receiver on the interface specified by the given address.
    ///
    /// If the given address is an IPv4 address then communication will only work between IPv4 devices, if the given address is IPv6 then communication
    /// will only work between IPv6 devices by default but IPv4 receiving can be enabled using set_ipv6_only(false).
    ///
    /// # Errors
    /// Will return an error if the SacnReceiver fails to bind to a socket with the given ip.
    /// For more details see socket2::Socket::new().
    ///
    pub fn new(ip: SocketAddr) -> Result<SacnNetworkReceiver> {
        Ok(SacnNetworkReceiver {
            socket: create_win_socket(ip)?,
            addr: ip,
            is_multicast_enabled: !(ip.is_ipv6()), // IPv6 Windows IP Multicast is currently unsupported.
        })
    }

    /// Connects this SacnNetworkReceiver to the multicast address which corresponds to the given universe to allow recieving packets for that universe.
    ///
    /// # Errors
    /// Will return an Error if the given universe cannot be converted to an Ipv4 or Ipv6 multicast_addr depending on if the Receiver is bound to an
    /// IPv4 or IPv6 address. See packet::universe_to_ipv4_multicast_addr and packet::universe_to_ipv6_multicast_addr.
    ///
    pub fn listen_multicast_universe(&self, universe: u16) -> Result<()> {
        let multicast_addr;

        if self.addr.is_ipv4() {
            multicast_addr = universe_to_ipv4_multicast_addr(universe)
                .chain_err(|| "Failed to convert universe to IPv4 multicast addr")?;
        } else {
            multicast_addr = universe_to_ipv6_multicast_addr(universe)
                .chain_err(|| "Failed to convert universe to IPv6 multicast addr")?;
        }

        Ok(join_win_multicast(&self.socket, multicast_addr)
            .chain_err(|| "Failed to join multicast")?)
    }

    /// Removes this SacnNetworkReceiver from the multicast group which corresponds to the given universe.
    ///
    /// # Errors
    /// Will return an Error if the given universe cannot be converted to an Ipv4 or Ipv6 multicast_addr depending on if the Receiver is bound to an
    /// IPv4 or IPv6 address. See packet::universe_to_ipv4_multicast_addr and packet::universe_to_ipv6_multicast_addr.
    ///
    pub fn mute_multicast_universe(&mut self, universe: u16) -> Result<()> {
        let multicast_addr;

        if self.addr.is_ipv4() {
            multicast_addr = universe_to_ipv4_multicast_addr(universe)
                .chain_err(|| "Failed to convert universe to IPv4 multicast addr")?;
        } else {
            multicast_addr = universe_to_ipv6_multicast_addr(universe)
                .chain_err(|| "Failed to convert universe to IPv6 multicast addr")?;
        }

        Ok(leave_win_multicast(&self.socket, multicast_addr)?)
    }

    /// Sets the value of the is_multicast_enabled flag to the given value.
    ///
    /// If set to false then the receiver won't attempt to join any more multicast groups.
    ///
    /// This method does not attempt to leave multicast groups already joined through previous listen_universe calls.
    ///
    /// # Arguments
    /// val: The new value for the is_multicast_enabled flag.
    ///
    /// # Errors
    /// Will return an OsOperationUnsupported error if attempting to set the flag to true in an environment that multicast
    /// isn't supported i.e. Ipv6 on Windows.
    pub fn set_is_multicast_enabled(&mut self, val: bool) -> Result<()> {
        if val && self.is_ipv6() {
            bail!(ErrorKind::OsOperationUnsupported(
                "IPv6 multicast is currently unsupported on Windows".to_string()
            ));
        }
        self.is_multicast_enabled = val;
        Ok(())
    }

    /// Returns true if multicast is enabled on this receiver and false if not.
    /// This flag is set when the receiver is created as not all environments currently support IP multicast.
    /// E.g. IPv6 Windows IP Multicast is currently unsupported.
    pub fn is_multicast_enabled(&self) -> bool {
        return self.is_multicast_enabled;
    }

    /// If set to true then only receieve over IPv6. If false then receiving will be over both IPv4 and IPv6.
    /// This will return an error if the SacnReceiver wasn't created using an IPv6 address to bind to.
    pub fn set_only_v6(&mut self, val: bool) -> Result<()> {
        if self.addr.is_ipv4() {
            bail!(IpVersionError(
                "No data avaliable in given timeout".to_string()
            ))
        } else {
            Ok(self.socket.set_only_v6(val)?)
        }
    }

    /// Returns a packet if there is one available.
    ///
    /// The packet may not be ready to transmit if it is awaiting synchronisation.
    /// Will only block if set_timeout was called with a timeout of None so otherwise (and by default) it won't
    /// block so may return a WouldBlock/TimedOut error to indicate that there was no data ready.
    ///
    /// IMPORTANT NOTE:
    /// An explicit lifetime is given to the AcnRootLayerProtocol which comes from the lifetime of the given buffer.
    /// The compiler will prevent usage of the returned AcnRootLayerProtocol after the buffer is dropped.
    ///
    /// Arguments:
    /// buf: The buffer to use for storing the received data into. This buffer shouldn't be accessed or used directly as the data
    /// is returned formatted properly in the AcnRootLayerProtocol. This buffer is used as memory space for the returned AcnRootLayerProtocol.
    ///
    /// # Errors
    /// May return an error if there is an issue receiving data from the underlying socket, see (recv)[fn.recv.Socket].
    ///
    /// May return an error if there is an issue parsing the data from the underlying socket, see (parse)[fn.AcnRootLayerProtocol::parse.packet].
    ///
    fn recv<'a>(
        &self,
        buf: &'a mut [u8; RCV_BUF_DEFAULT_SIZE],
    ) -> Result<AcnRootLayerProtocol<'a>> {
        self.socket.recv(&mut buf[0..])?;

        Ok(AcnRootLayerProtocol::parse(buf)?)
    }

    /// Set the timeout for the recv operation.
    ///
    /// Arguments:
    /// timeout: The new timeout for the receive operation, a value of None means the recv operation will become blocking.
    ///
    /// Errors:
    /// A timeout with Duration 0 will cause an error. See (set_read_timeout)[fn.set_read_timeout.Socket].
    ///
    pub fn set_timeout(&mut self, timeout: Option<Duration>) -> Result<()> {
        Ok(self.socket.set_read_timeout(timeout)?)
    }

    /// Returns true if this SacnNetworkReceiver is bound to an Ipv6 address.
    pub fn is_ipv6(&self) -> bool {
        return self.addr.is_ipv6();
    }
}

/// Windows and linux handle multicast sockets differently.
/// This is built for / tested with Fedora 30/31.
#[cfg(target_os = "linux")]
impl SacnNetworkReceiver {
    /// Creates a new DMX receiver on the interface specified by the given address.
    ///
    /// If the given address is an IPv4 address then communication will only work between IPv4 devices, if the given address is IPv6 then communication
    /// will only work between IPv6 devices by default but IPv4 receiving can be enabled using set_ipv6_only(false).
    ///
    /// # Errors
    /// Will return an error if the SacnReceiver fails to bind to a socket with the given ip.
    /// For more details see socket2::Socket::new().
    ///
    pub fn new(ip: SocketAddr) -> Result<SacnNetworkReceiver> {
        Ok(SacnNetworkReceiver {
            socket: create_unix_socket(ip)?,
            addr: ip,
            is_multicast_enabled: true, // Linux IP Multicast is supported for Ipv4 and Ipv6.
        })
    }

    /// Connects this SacnNetworkReceiver to the multicast address which corresponds to the given universe to allow recieving packets for that universe.
    ///
    /// # Errors
    /// Will return an Error if the given universe cannot be converted to an Ipv4 or Ipv6 multicast_addr depending on if the Receiver is bound to an
    /// IPv4 or IPv6 address. See packet::universe_to_ipv4_multicast_addr and packet::universe_to_ipv6_multicast_addr.
    ///
    pub fn listen_multicast_universe(&self, universe: u16) -> Result<()> {
        let multicast_addr;

        if self.addr.is_ipv4() {
            multicast_addr = universe_to_ipv4_multicast_addr(universe)
                .chain_err(|| "Failed to convert universe to IPv4 multicast addr")?;
        } else {
            multicast_addr = universe_to_ipv6_multicast_addr(universe)
                .chain_err(|| "Failed to convert universe to IPv6 multicast addr")?;
        }

        Ok(
            join_unix_multicast(&self.socket, multicast_addr, self.addr.ip())
                .chain_err(|| "Failed to join multicast")?,
        )
    }

    /// Removes this SacnNetworkReceiver from the multicast group which corresponds to the given universe.
    ///
    /// # Errors
    /// Will return an Error if the given universe cannot be converted to an Ipv4 or Ipv6 multicast_addr depending on if the Receiver is bound to an
    /// IPv4 or IPv6 address. See packet::universe_to_ipv4_multicast_addr and packet::universe_to_ipv6_multicast_addr.
    ///
    pub fn mute_multicast_universe(&mut self, universe: u16) -> Result<()> {
        let multicast_addr;

        if self.addr.is_ipv4() {
            multicast_addr = universe_to_ipv4_multicast_addr(universe)
                .chain_err(|| "Failed to convert universe to IPv4 multicast addr")?;
        } else {
            multicast_addr = universe_to_ipv6_multicast_addr(universe)
                .chain_err(|| "Failed to convert universe to IPv6 multicast addr")?;
        }

        Ok(leave_unix_multicast(
            &self.socket,
            multicast_addr,
            self.addr.ip(),
        )?)
    }

    /// Sets the value of the is_multicast_enabled flag to the given value.
    ///
    /// If set to false then the receiver won't attempt to join any more multicast groups.
    ///
    /// This method does not attempt to leave multicast groups already joined through previous listen_universe calls.
    ///
    /// # Arguments
    /// val: The new value for the is_multicast_enabled flag.
    ///
    /// # Errors
    /// Will return an OsOperationUnsupported error if attempting to set the flag to true in an environment that multicast
    /// isn't supported i.e. Ipv6 on Windows. Note that this is the UNIX implementation
    pub fn set_is_multicast_enabled(&mut self, val: bool) -> Result<()> {
        self.is_multicast_enabled = val;
        Ok(())
    }

    /// Returns true if multicast is enabled on this receiver and false if not.
    /// This flag is set when the receiver is created as not all environments currently support IP multicast.
    /// E.g. IPv6 Windows IP Multicast is currently unsupported.
    pub fn is_multicast_enabled(&self) -> bool {
        return self.is_multicast_enabled;
    }

    /// If set to true then only receieve over IPv6. If false then receiving will be over both IPv4 and IPv6.
    /// This will return an error if the SacnReceiver wasn't created using an IPv6 address to bind to.
    pub fn set_only_v6(&mut self, val: bool) -> Result<()> {
        if self.addr.is_ipv4() {
            bail!(IpVersionError(
                "No data avaliable in given timeout".to_string()
            ))
        } else {
            Ok(self.socket.set_only_v6(val)?)
        }
    }

    /// Returns a packet if there is one available.
    ///
    /// The packet may not be ready to transmit if it is awaiting synchronisation.
    /// Will only block if set_timeout was called with a timeout of None so otherwise (and by default) it won't
    /// block so may return a WouldBlock/TimedOut error to indicate that there was no data ready.
    ///
    /// IMPORTANT NOTE:
    /// An explicit lifetime is given to the AcnRootLayerProtocol which comes from the lifetime of the given buffer.
    /// The compiler will prevent usage of the returned AcnRootLayerProtocol after the buffer is dropped.
    ///
    /// Arguments:
    /// buf: The buffer to use for storing the received data into. This buffer shouldn't be accessed or used directly as the data
    /// is returned formatted properly in the AcnRootLayerProtocol. This buffer is used as memory space for the returned AcnRootLayerProtocol.
    ///
    /// # Errors
    /// May return an error if there is an issue receiving data from the underlying socket, see (recv)[fn.recv.Socket].
    ///
    /// May return an error if there is an issue parsing the data from the underlying socket, see (parse)[fn.AcnRootLayerProtocol::parse.packet].
    ///
    fn recv<'a>(
        &self,
        buf: &'a mut [u8; RCV_BUF_DEFAULT_SIZE],
    ) -> Result<AcnRootLayerProtocol<'a>> {
        self.socket.recv(&mut buf[0..])?;

        Ok(AcnRootLayerProtocol::parse(buf)?)
    }

    /// Set the timeout for the recv operation.
    ///
    /// Arguments:
    /// timeout: The new timeout for the receive operation, a value of None means the recv operation will become blocking.
    ///
    /// Errors:
    /// A timeout with Duration 0 will cause an error. See (set_read_timeout)[fn.set_read_timeout.Socket].
    ///
    pub fn set_timeout(&mut self, timeout: Option<Duration>) -> Result<()> {
        Ok(self.socket.set_read_timeout(timeout)?)
    }
}

impl Clone for DMXData {
    fn clone(&self) -> DMXData {
        let new_vals = self.values.to_vec(); // https://stackoverflow.com/questions/21369876/what-is-the-idiomatic-rust-way-to-copy-clone-a-vector-in-a-parameterized-functio (26/12/2019)
        DMXData {
            universe: self.universe,
            values: new_vals,
            sync_uni: self.sync_uni,
            priority: self.priority,
            src_cid: self.src_cid,
            preview: self.preview,
        }
    }
}

/// DMXData has a total ordering based on the unvierse, then sync-universe and finally values.
impl Ord for DMXData {
    fn cmp(&self, other: &Self) -> Ordering {
        self.universe
            .cmp(&other.universe)
            .then(self.sync_uni.cmp(&other.sync_uni))
            .then(self.values.cmp(&other.values))
    }
}

/// See Ord trait implementation for DMXData.
impl PartialOrd for DMXData {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// DMXData is taken to be equivalent iff:
///     - The universes are the same
///     - The synchronisation universes are the same
///     - The values are all the same
impl PartialEq for DMXData {
    fn eq(&self, other: &Self) -> bool {
        self.universe == other.universe
            && self.sync_uni == other.sync_uni
            && self.values == other.values
    }
}

/// See PartialEq trait implementation for DMXData.
impl Eq for DMXData {}

impl DiscoveredSacnSource {
    /// Returns true if all the pages sent by this DiscoveredSacnSource have been receieved.
    ///
    /// This is based on each page containing a last-page value which indicates the number of the last page expected.
    pub fn has_all_pages(&mut self) -> bool {
        // https://rust-lang-nursery.github.io/rust-cookbook/algorithms/sorting.html (31/12/2019)
        self.pages.sort_by(|a, b| a.page.cmp(&b.page));
        for i in 0..(self.last_page + 1) {
            if self.pages[i as usize].page != i {
                return false;
            }
        }

        return true;
    }

    /// Returns all the universes being send by this SacnSource as discovered through the universe discovery mechanism.
    ///
    /// Intentionally abstracts over the underlying concept of pages as this is purely an E1.31 Universe Discovery concept and is otherwise transparent.
    pub fn get_all_universes(&self) -> Vec<u16> {
        let mut uni: Vec<u16> = Vec::new();
        for p in &self.pages {
            uni.extend_from_slice(&p.universes);
        }
        uni
    }

    /// Removes the given universe from the list of universes being sent by this discovered source.
    pub fn terminate_universe(&mut self, universe: u16) {
        for p in &mut self.pages {
            p.universes.retain(|x| *x != universe)
        }
    }
}

/// Creates a new Socket2 socket bound to the given address.
///
/// Returns the created socket.
///
/// Arguments:
/// addr: The address that the newly created socket should bind to.
///
/// # Errors
/// Will return an error if the socket cannot be created, see (Socket::new)[fn.new.Socket].
///
/// Will return an error if the socket cannot be bound to the given address, see (bind)[fn.bind.Socket2].
#[cfg(target_os = "linux")]
fn create_unix_socket(addr: SocketAddr) -> Result<Socket> {
    if addr.is_ipv4() {
        let socket = Socket::new(Domain::ipv4(), Type::dgram(), Some(Protocol::udp()))?;

        // Multiple different processes might want to listen to the sACN stream so therefore need to allow re-using the ACN port.
        socket.set_reuse_port(true)?;
        socket.set_reuse_address(true)?;

        let socket_addr =
            SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), ACN_SDT_MULTICAST_PORT);
        socket.bind(&socket_addr.into())?;
        Ok(socket)
    } else {
        let socket = Socket::new(Domain::ipv6(), Type::dgram(), Some(Protocol::udp()))?;

        // Multiple different processes might want to listen to the sACN stream so therefore need to allow re-using the ACN port.
        socket.set_reuse_port(true)?;
        socket.set_reuse_address(true)?;

        let socket_addr =
            SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), ACN_SDT_MULTICAST_PORT);
        socket.bind(&socket_addr.into())?;
        Ok(socket)
    }
}

/// Joins the multicast group with the given address using the given socket.
///
/// Arguments:
/// socket: The socket to join to the multicast group.
/// addr:   The address of the multicast group to join.
///
/// # Errors
/// Will return an error if the given socket cannot be joined to the given multicast group address.
///     See join_multicast_v4[fn.join_multicast_v4.Socket] and join_multicast_v6[fn.join_multicast_v6.Socket]
///
/// Will return an IpVersionError if addr and interface_addr are not the same IP version.
///
#[cfg(target_os = "linux")]
fn join_unix_multicast(socket: &Socket, addr: SockAddr, interface_addr: IpAddr) -> Result<()> {
    match addr.family() as i32 {
        // Cast required because AF_INET is defined in libc in terms of a c_int (i32) but addr.family returns using u16.
        AF_INET => match addr.as_inet() {
            Some(a) => match interface_addr {
                IpAddr::V4(ref interface_v4) => {
                    socket
                        .join_multicast_v4(a.ip(), &interface_v4)
                        .chain_err(|| "Failed to join IPv4 multicast")?;
                }
                IpAddr::V6(ref _interface_v6) => {
                    bail!(ErrorKind::IpVersionError(
                        "Multicast address and interface_addr not same IP version".to_string()
                    ));
                }
            },
            None => {
                bail!(ErrorKind::UnsupportedIpVersion("IP version recognised as AF_INET but not actually usable as AF_INET so must be unknown type".to_string()));
            }
        },
        AF_INET6 => match addr.as_inet6() {
            Some(a) => {
                socket
                    .join_multicast_v6(a.ip(), 0)
                    .chain_err(|| "Failed to join IPv6 multicast")?;
            }
            None => {
                bail!(ErrorKind::UnsupportedIpVersion("IP version recognised as AF_INET6 but not actually usable as AF_INET6 so must be unknown type".to_string()));
            }
        },
        x => {
            bail!(ErrorKind::UnsupportedIpVersion(format!("IP version not recognised as AF_INET (Ipv4) or AF_INET6 (Ipv6) - family value (as i32): {}", x).to_string()));
        }
    };

    Ok(())
}

/// Leaves the multicast group with the given address using the given socket.
///
/// Arguments:
/// socket: The socket to leave the multicast group.
/// addr:   The address of the multicast group to leave.
///
/// # Errors
/// Will return an error if the given socket cannot leave the given multicast group address.
///     See leave_multicast_v4[fn.leave_multicast_v4.Socket] and leave_multicast_v6[fn.leave_multicast_v6.Socket]
///
/// Will return an IpVersionError if addr and interface_addr are not the same IP version.
///
#[cfg(target_os = "linux")]
fn leave_unix_multicast(socket: &Socket, addr: SockAddr, interface_addr: IpAddr) -> Result<()> {
    match addr.family() as i32 {
        // Cast required because AF_INET is defined in libc in terms of a c_int (i32) but addr.family returns using u16.
        AF_INET => match addr.as_inet() {
            Some(a) => match interface_addr {
                IpAddr::V4(ref interface_v4) => {
                    socket
                        .leave_multicast_v4(a.ip(), &interface_v4)
                        .chain_err(|| "Failed to leave IPv4 multicast")?;
                }
                IpAddr::V6(ref _interface_v6) => {
                    bail!(ErrorKind::IpVersionError(
                        "Multicast address and interface_addr not same IP version".to_string()
                    ));
                }
            },
            None => {
                bail!(ErrorKind::UnsupportedIpVersion("IP version recognised as AF_INET but not actually usable as AF_INET so must be unknown type".to_string()));
            }
        },
        AF_INET6 => match addr.as_inet6() {
            Some(a) => {
                socket
                    .leave_multicast_v6(a.ip(), 0)
                    .chain_err(|| "Failed to leave IPv6 multicast")?;
            }
            None => {
                bail!(ErrorKind::UnsupportedIpVersion("IP version recognised as AF_INET6 but not actually usable as AF_INET6 so must be unknown type".to_string()));
            }
        },
        x => {
            bail!(ErrorKind::UnsupportedIpVersion(format!("IP version not recognised as AF_INET (Ipv4) or AF_INET6 (Ipv6) - family value (as i32): {}", x).to_string()));
        }
    };

    Ok(())
}

/// Creates a new Socket2 socket bound to the given address.
///
/// Returns the created socket.
///
/// Arguments:
/// addr: The address that the newly created socket should bind to.
///
/// # Errors
/// Will return an error if the socket cannot be created, see (Socket::new)[fn.new.Socket].
///
/// Will return an error if the socket cannot be bound to the given address, see (bind)[fn.bind.Socket].
#[cfg(target_os = "windows")]
fn create_win_socket(addr: SocketAddr) -> Result<Socket> {
    if addr.is_ipv4() {
        let socket = Socket::new(Domain::ipv4(), Type::dgram(), Some(Protocol::udp()))?;

        socket.set_reuse_address(true)?;
        socket.bind(&SockAddr::from(addr))?;
        Ok(socket)
    } else {
        let socket = Socket::new(Domain::ipv6(), Type::dgram(), Some(Protocol::udp()))?;

        socket.set_reuse_address(true)?;
        socket.bind(&SockAddr::from(addr))?;
        Ok(socket)
    }
}

/// Joins the multicast group with the given address using the given socket on the windows operating system.
///
/// Note that Ipv6 is currently unsupported.
///
/// Arguments:
/// socket: The socket to join to the multicast group.
/// addr:   The address of the multicast group to join.
///
/// # Errors
/// Will return an error if the given socket cannot be joined to the given multicast group address.
///     See join_multicast_v4[fn.join_multicast_v4.Socket] and join_multicast_v6[fn.join_multicast_v6.Socket]
///
/// Will return OsOperationUnsupported error if attempt to leave an Ipv6 multicast group as all Ipv6 multicast operations are currently unsupported in Rust on Windows.
///
#[cfg(target_os = "windows")]
fn join_win_multicast(socket: &Socket, addr: SockAddr) -> Result<()> {
    match addr.family() as i32 {
        // Cast required because AF_INET is defined in libc in terms of a c_int (i32) but addr.family returns using u16.
        AF_INET => match addr.as_inet() {
            Some(a) => {
                socket
                    .join_multicast_v4(a.ip(), &Ipv4Addr::new(0, 0, 0, 0))
                    .chain_err(|| "Failed to join IPv4 multicast")?;
            }
            None => {
                bail!(ErrorKind::UnsupportedIpVersion("IP version recognised as AF_INET but not actually usable as AF_INET so must be unknown type".to_string()));
            }
        },
        AF_INET6 => match addr.as_inet6() {
            Some(_) => {
                bail!(ErrorKind::OsOperationUnsupported(
                    "IPv6 multicast is currently unsupported on Windows".to_string()
                ));
            }
            None => {
                bail!(ErrorKind::UnsupportedIpVersion("IP version recognised as AF_INET6 but not actually usable as AF_INET6 so must be unknown type".to_string()));
            }
        },
        x => {
            bail!(ErrorKind::UnsupportedIpVersion(format!("IP version not recognised as AF_INET (Ipv4) or AF_INET6 (Ipv6) - family value (as i32): {}", x).to_string()));
        }
    };

    Ok(())
}

/// Leaves the multicast group with the given address using the given socket.
///
/// Note that Ipv6 is currently unsupported.
///
/// Arguments:
/// socket: The socket to leave the multicast group.
/// addr:   The address of the multicast group to leave.
///
/// # Errors
/// Will return an error if the given socket cannot leave the given multicast group address.
///     See leave_multicast_v4[fn.leave_multicast_v4.Socket] and leave_multicast_v6[fn.leave_multicast_v6.Socket]
///
/// Will return OsOperationUnsupported error if attempt to leave an Ipv6 multicast group as all Ipv6 multicast operations are currently unsupported in Rust on Windows.
///
#[cfg(target_os = "windows")]
fn leave_win_multicast(socket: &Socket, addr: SockAddr) -> Result<()> {
    match addr.family() as i32 {
        // Cast required because AF_INET is defined in libc in terms of a c_int (i32) but addr.family returns using u16.
        AF_INET => match addr.as_inet() {
            Some(a) => {
                socket
                    .leave_multicast_v4(a.ip(), &Ipv4Addr::new(0, 0, 0, 0))
                    .chain_err(|| "Failed to leave IPv4 multicast")?;
            }
            None => {
                bail!(ErrorKind::UnsupportedIpVersion("IP version recognised as AF_INET but not actually usable as AF_INET so must be unknown type".to_string()));
            }
        },
        AF_INET6 => match addr.as_inet6() {
            Some(_) => {
                bail!(ErrorKind::OsOperationUnsupported(
                    "IPv6 multicast is currently unsupported on Windows".to_string()
                ));
            }
            None => {
                bail!(ErrorKind::UnsupportedIpVersion("IP version recognised as AF_INET6 but not actually usable as AF_INET6 so must be unknown type".to_string()));
            }
        },
        x => {
            bail!(ErrorKind::UnsupportedIpVersion(format!("IP version not recognised as AF_INET (Ipv4) or AF_INET6 (Ipv6) - family value (as i32): {}", x).to_string()));
        }
    };

    Ok(())
}

/// Stores a sequence number and a timestamp.
/// 
/// Used internally within SequenceNumbering for tracking the last received timestamps of each packet-type, source, universe combination.
/// 
/// This is then used to workout timeouts to trigger network data loss as per ANSI E1.31-2018 Section 6.7.1.
/// 
#[derive(Copy, Clone)]
struct TimedStampedSeqNo {
    sequence_number: u8,
    last_recv: Instant, 
}

impl TimedStampedSeqNo {
    fn new(sequence_number: u8, last_recv: Instant) -> TimedStampedSeqNo{
        TimedStampedSeqNo {
            sequence_number: sequence_number,
            last_recv: last_recv
        }
    }
}

/// Stores information about the current expected sequence numbers for each source, universe and packet type.
/// 
/// Also handles timeouts of sources.
/// 
/// Abstracts over the internal data-structures/mechanisms used allowing them be changed.
/// 
struct SequenceNumbering {
    /// The sequence numbers used for data packets, keeps a reference of the last sequence number received for each universe.
    /// Sequence numbers are always in the range [0, 255] inclusive.
    /// Each type of packet is tracked differently with respect to sequence numbers as per ANSI E1.31-2018 Section 6.7.2 Sequence Numbering.
    /// The uuid refers to the source that is sending the data.
    data_sequences: HashMap<Uuid, HashMap<u16, TimedStampedSeqNo>>,

    /// The sequence numbers used for synchronisation packets, keeps a reference of the last sequence number received for each universe.
    /// Sequence numbers are always in the range [0, 255] inclusive.
    /// Each type of packet is tracked differently with respect to sequence numbers as per ANSI E1.31-2018 Section 6.7.2 Sequence Numbering.
    /// The uuid refers to the source that is sending the data.
    sync_sequences: HashMap<Uuid, HashMap<u16, TimedStampedSeqNo>>,

}

impl SequenceNumbering {
    /// Creates a new SequenceNumbering for tracking sequence numbers for the various types of packets.
    /// 
    /// This implementation uses HashMaps internally to allow O(1) checking and updating of sequence numbers.
    /// 
    fn new() -> SequenceNumbering{
        return SequenceNumbering {
            data_sequences: HashMap::new(),
            sync_sequences: HashMap::new()
        }
    }

    /// Clears the sequence number records completely removing all sources/universes for all types of packet.
    /// 
    fn clear(&mut self) {
        self.data_sequences.clear();
        self.sync_sequences.clear();
    }

    /// Checks the sequence number is correct for a data packet with the given sequence_number and universe from the given source with given cid.
    /// Uses the given source_limit to check that it isn't exceeded.
    /// 
    /// Returns Ok(()) if the packet is detected in-order.
    ///
    /// # Arguments
    /// source_limit: The limit on the number of sources which are allowed, None indicates no limit, if there is a limit then a SourcesExceededError may be returned.
    /// 
    /// cid:    The Uuid of the source that send the packet.
    /// 
    /// sequence_number: The sequence number of the packet to check.
    /// 
    /// universe: The data niverse of the packet.
    ///
    /// # Errors
    /// Returns an OutOfSequence error if a packet is received out of order as detected by the different between
    /// the packets sequence number and the expected sequence number as specified in ANSI E1.31-2018 Section 6.7.2 Sequence Numbering.
    ///
    /// Return a SourcesExceededError if the cid of the source is new and would cause the number of sources to exceed the given source_limit.
    ///
    fn check_data_seq_number(&mut self, source_limit: Option<usize>, cid: Uuid, sequence_number: u8, universe: u16, announce_timeout: bool) -> Result<()>{
        check_seq_number(&mut self.data_sequences, source_limit, cid, sequence_number, universe, announce_timeout)
    }

    /// Checks the sequence number is correct for a sync packet with the given sequence_number and universe from the given source with given cid.
    /// Uses the given source_limit to check that it isn't exceeded.
    /// 
    /// Returns Ok(()) if the packet is detected in-order.
    ///
    /// # Arguments
    /// source_limit: The limit on the number of sources which are allowed, None indicates no limit, if there is a limit then a SourcesExceededError may be returned.
    /// 
    /// cid:    The Uuid of the source that send the packet.
    /// 
    /// sequence_number: The sequence number of the packet to check.
    /// 
    /// universe: The sync universe of the packet
    ///
    /// # Errors
    /// Returns an OutOfSequence error if a packet is received out of order as detected by the different between
    /// the packets sequence number and the expected sequence number as specified in ANSI E1.31-2018 Section 6.7.2 Sequence Numbering.
    ///
    /// Return a SourcesExceededError if the cid of the source is new and would cause the number of sources to exceed the given source_limit.
    ///
    fn check_sync_seq_number(&mut self, source_limit: Option<usize>, cid: Uuid, sequence_number: u8, sync_uni: u16, announce_timeout: bool) -> Result<()>{
        check_seq_number(&mut self.sync_sequences, source_limit, cid, sequence_number, sync_uni, announce_timeout)
    }

    /// Removes the sequence number tracking for the given source / universe combination.
    /// This applies to both data and sync packets.
    /// 
    /// # Arguments:
    /// 
    /// src_cid: The CID of the source to remove the sequence numbers of.
    /// 
    /// universe: The universe being sent by the source from which to remove the sequence numbers.
    /// 
    fn remove_seq_numbers<'a>(&mut self, src_cid: Uuid, universe: u16) -> Result<()> {
        remove_source_universe_seq(&mut self.data_sequences, src_cid, universe)?;
        remove_source_universe_seq(&mut self.sync_sequences, src_cid, universe)
    }
}

/// Checks the given sequence number for the given universe against the given expected sequence numbers.
///
/// Returns Ok(()) if the packet is detected in-order.
///
/// # Arguments
/// src_sequences: A mutable hashmap which relates sources identified by Uuid to another hashmap which itself relates universes to sequence numbers. The given hashmap of
///                 sequences should be for the specific packet-type being checked as different packet-types have their own sequence numbers even from the same source.
/// source_limit: The limit on the number of sources which are allowed, None indicates no limit, if there is a limit then a SourcesExceededError may be returned.
/// cid:    The Uuid of the source that send the packet.
/// sequence_number: The sequence number of the packet to check.
/// universe: The universe of the packet (this is the data universe for data packets and the sync universe for synchronisation packets).
///
/// # Errors
/// Returns an OutOfSequence error if a packet is received out of order as detected by the different between
/// the packets sequence number and the expected sequence number as specified in ANSI E1.31-2018 Section 6.7.2 Sequence Numbering.
///
/// Return a SourcesExceededError if the cid of the source is new and would cause the number of sources to exceed the given source_limit.
///
fn check_seq_number(
    src_sequences: &mut HashMap<Uuid, HashMap<u16, TimedStampedSeqNo>>,
    source_limit: Option<usize>,
    cid: Uuid,
    sequence_number: u8,
    universe: u16,
    announce_timeout: bool,
) -> Result<()> {

    // Check all the timeouts at the start.
    // This is done for all sources/universes rather than just the source that sent the packet because a completely dead (no packets being sent) universe 
    // would not be removed otherwise and would continue to take up space. This comes at the cost of increased processing time complexity as each
    // source is checked every time.
    check_timeouts(src_sequences, E131_NETWORK_DATA_LOSS_TIMEOUT, announce_timeout)?;

    match src_sequences.get(&cid) {
        None => {
            // New source not previously received from.
            if source_limit.is_none() || src_sequences.len() < source_limit.unwrap() {
                src_sequences.insert(cid, HashMap::new());
            } else {
                bail!(ErrorKind::SourcesExceededError(
                    format!("Already at max sources: {}", src_sequences.len()).to_string()
                ));
            }
        }
        Some(_) => {}
    };

    let expected_seq = match src_sequences.get(&cid) {
        Some(src) => {
            let seq_num = match src.get(&universe) { // Get the sequence number within the source for the specific universe.
                Some(s) => { // Indicates that the source / universe combination is known.
                    *s
                },
                None => { // Indicates that this is the first time (or the first time since it timed out) the universe has been received from this source.
                    TimedStampedSeqNo::new(INITIAL_SEQUENCE_NUMBER, Instant::now())
                }
            };
            seq_num
        }
        None => {
            // Previously checked that cid is present (and added if not), if None is returned now it indicates that between that check and this 
            // function the cid key value has been removed. This can only happen if there is a memory corruption/thread-iterleaving or similar external
            // event which the receiver cannot handle.
            panic!();
        }
    };

    let seq_diff: isize = (sequence_number as isize) - (expected_seq.sequence_number as isize);

    if seq_diff <= E131_SEQ_DIFF_DISCARD_UPPER_BOUND && seq_diff > E131_SEQ_DIFF_DISCARD_LOWER_BOUND
    {
        // Reject the out of order packet as per ANSI E1.31-2018 Section 6.7.2 Sequence Numbering.
        bail!(ErrorKind::OutOfSequence(
            format!(
                "Packet recieved with sequence number {} is out of sequence, last {}, seq-diff {}",
                sequence_number, expected_seq.sequence_number, seq_diff
            )
            .to_string()
        ));
    }

    match src_sequences.get_mut(&cid) {
        Some(src) => {
            // Replace the old sequence number with the new and reset the timeout.
            src.insert(universe, TimedStampedSeqNo::new(sequence_number, Instant::now()));
        }
        None => {
            panic!();
        }
    };

    Ok(())
}

/// Checks the timeouts for all sources and universes for the given sequences.
/// Removes any universes for which the last_recv time was at least the given timeout amount of time ago.
/// Any sources which have no universes after this operation are also removed.
/// 
/// #Arguments
/// 
/// src_sequences: The source sequence numbers to check the timeout of.
/// 
/// timeout: The exclusive length of time permitted since a source last sent on a universe. 
///     If the time elapsed since the last received data that is equal to or great than the timeout then the source is said to have timed out.
///  
fn check_timeouts(src_sequences: &mut HashMap<Uuid, HashMap<u16, TimedStampedSeqNo>>, timeout: Duration, announce_timeout: bool) -> Result<()> {
    if announce_timeout {
        let mut timedout_src_id: Option<Uuid> = None;
        let mut timedout_uni: Option<u16> = None;
        for (src_id, universes) in src_sequences.iter_mut() {
            for (uni, seq_num) in universes.iter() {
                if seq_num.last_recv.elapsed() >= timeout {
                    timedout_src_id = Some(*src_id);
                    timedout_uni = Some(*uni);
                    break;
                }
            }
            if timedout_uni == None {
                break;
            }
        }

        if timedout_uni.is_some() { // If None then it indicates nothing timed out.
            let uni_to_remove = timedout_uni.unwrap();
            let src_universes = src_sequences.get_mut(&timedout_src_id.unwrap());
            if src_universes.is_some() {
                let universes = src_universes.unwrap();
                universes.remove(&uni_to_remove);
                if universes.is_empty() {
                    src_sequences.remove(&timedout_src_id.unwrap());
                    bail!(ErrorKind::UniverseTimeout("Timed out".to_string()));
                }
            }
        }

        Ok(())
    } else {
        for (_src_id, universes) in src_sequences.iter_mut() {
            universes.retain(|_uni, seq_num| seq_num.last_recv.elapsed() < timeout);
        }
    
        // Remove all empty sources.
        src_sequences.retain(|_src_id, universes| !universes.is_empty());
        Ok(())
    }
}

/// Removes the sequence number entry from the given sequences for the given source cid and universe.
/// 
/// This removes the source entirely if there are no universes left.
/// 
/// # Arguments
/// src_sequences: The sequence numbers for each source and universe.
/// 
/// src_cid:       The CID for the source to remove the unvierse from.
/// 
/// universe:      The universe to remove from the source.
/// 
/// # Errors
/// Returns a SourceNotFound error if the given src_cid isn't in the given collection of sources/sequence-numbers.
/// 
/// Returns a UniverseNotFound error if the given universe isn't registered to the given source and so cannot be removed.
/// 
fn remove_source_universe_seq(src_sequences: &mut HashMap<Uuid, HashMap<u16, TimedStampedSeqNo>>, src_cid: Uuid, universe: u16) -> Result<()> {
    match src_sequences.get_mut(&src_cid) {
        Some(x) => {
            match x.remove(&universe) {
                Some(_) => {
                    if x.is_empty() { // Remove the source if there are no universes registered to it.
                        match src_sequences.remove(&src_cid) {
                            Some (_x) => {
                                Ok(())
                            }
                            None => {
                                bail!(ErrorKind::SourceNotFound("Could not find the source so could not remove it".to_string()));
                            }
                        }
                    } else {
                        Ok(())
                    }
                }
                None => {
                    bail!(ErrorKind::UniverseNotFound("Could not find universe within source in sequence numbers so could not remove it".to_string()));
                }
            }
        }
        None => {
            bail!(ErrorKind::SourceNotFound("Could not find the source in the sequence numbers so could not remove it".to_string()));
        }
    }
}


/// The default merge action for the receiver.
///
/// This discarding of the old data is the default action for compliance as specified in ANSI E1.31-2018, Section 11.2.1.
///
/// This can be changed if required as part of the mechanism described in ANSI E1.31-2018, Section 6.2.3.4 Requirements for Merging and Arbitrating.
///
/// The first argument (i) is the existing data, n is the new data.
/// This function is only valid if both inputs have the same universe, sync addr, start_code and the data contains at least the first value (the start code).
/// If this doesn't hold an error will be returned.
/// Other merge functions may allow merging different start codes or not check for them.
pub fn discard_lowest_priority_then_previous(i: &DMXData, n: &DMXData) -> Result<DMXData> {
    if i.priority > n.priority {
        return Ok(i.clone());
    }
    Ok(n.clone())
}

/// Performs a highest takes priority (HTP) (per byte) DMX merge of data.
///
/// Note this merge is done within the explicit priorty, if i or n has an explictly higher priority it will always take precedence before this HTP merge is attempted.
/// If either data has the preview flag set then the result will have the preview flag set.
/// 
/// Given as an example of a possible merge algorithm.
/// 
/// The first argument (i) is the existing data, n is the new data.
/// 
/// This function is only valid if both inputs have the same universe, sync addr, start_code and the data contains at least the first value (the start code).
/// If this doesn't hold an error will be returned.
/// Other merge functions may allow merging different start codes or not check for them.
pub fn htp_dmx_merge(i: &DMXData, n: &DMXData) -> Result<DMXData> {
    if i.values.len() < 1
        || n.values.len() < 1
        || i.universe != n.universe
        || i.values[0] != n.values[0]
        || i.sync_uni != n.sync_uni
    {
        bail!(DmxMergeError("Attempted DMX merge on dmx data with different universes, syncronisation universes or data with no values".to_string()));
    }

    if i.priority > n.priority {
        return Ok(i.clone());
    } else if n.priority > i.priority {
        return Ok(n.clone());
    }
    // Must have same priority.

    let mut r: DMXData = DMXData {
        universe: i.universe,
        values: Vec::new(),
        sync_uni: i.sync_uni,
        priority: i.priority,
        src_cid: None,
        preview: i.preview || n.preview // If either data is preview then mark the result as preview.
    };

    let mut i_iter = i.values.iter();
    let mut n_iter = n.values.iter();

    let mut i_val = i_iter.next();
    let mut n_val = n_iter.next();

    while (i_val.is_some()) || (n_val.is_some()) {
        if i_val == None {
            r.values.push(*n_val.unwrap());
        } else if n_val == None {
            r.values.push(*i_val.unwrap());
        } else {
            r.values.push(max(*n_val.unwrap(), *i_val.unwrap()));
        }

        i_val = i_iter.next();
        n_val = n_iter.next();
    }

    Ok(r)
}

#[test]
fn test_handle_single_page_discovery_packet() {
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), ACN_SDT_MULTICAST_PORT);

    let mut dmx_rcv = SacnReceiver::with_ip(addr, None).unwrap();

    let name = "Test Src 1";
    let page: u8 = 0;
    let last_page: u8 = 0;
    let universes: Vec<u16> = vec![0, 1, 2, 3, 4, 5];

    let discovery_pkt: UniverseDiscoveryPacketFramingLayer = UniverseDiscoveryPacketFramingLayer {
        source_name: name.into(),

        /// Universe discovery layer.
        data: UniverseDiscoveryPacketUniverseDiscoveryLayer {
            page: page,

            /// The number of the final page.
            last_page: last_page,

            /// List of universes.
            universes: universes.clone().into(),
        },
    };
    let res: bool = dmx_rcv.handle_universe_discovery_packet(discovery_pkt);

    assert!(res);

    assert_eq!(dmx_rcv.discovered_sources.len(), 1);

    assert_eq!(dmx_rcv.discovered_sources[0].name, name);
    assert_eq!(dmx_rcv.discovered_sources[0].last_page, last_page);
    assert_eq!(dmx_rcv.discovered_sources[0].pages.len(), 1);
    assert_eq!(dmx_rcv.discovered_sources[0].pages[0].page, page);
    assert_eq!(dmx_rcv.discovered_sources[0].pages[0].universes, universes);
}

#[test]
fn test_handle_multi_page_discovery_packet() {
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), ACN_SDT_MULTICAST_PORT);

    let mut dmx_rcv = SacnReceiver::with_ip(addr, None).unwrap();

    let name = "Test Src 1";
    let last_page: u8 = 1;
    let mut universes_page_1: Vec<u16> = Vec::new();
    let mut universes_page_2: Vec<u16> = Vec::new();

    for i in 1..513 {
        universes_page_1.push(i);
    }

    for i in 513..1024 {
        universes_page_2.push(i);
    }

    let discovery_pkt_1: UniverseDiscoveryPacketFramingLayer =
        UniverseDiscoveryPacketFramingLayer {
            source_name: name.into(),

            /// Universe discovery layer.
            data: UniverseDiscoveryPacketUniverseDiscoveryLayer {
                page: 0,

                /// The number of the final page.
                last_page: last_page,

                /// List of universes.
                universes: universes_page_1.clone().into(),
            },
        };

    let discovery_pkt_2: UniverseDiscoveryPacketFramingLayer =
        UniverseDiscoveryPacketFramingLayer {
            source_name: name.into(),

            /// Universe discovery layer.
            data: UniverseDiscoveryPacketUniverseDiscoveryLayer {
                page: 1,

                /// The number of the final page.
                last_page: last_page,

                /// List of universes.
                universes: universes_page_2.clone().into(),
            },
        };
    let res1: bool = dmx_rcv.handle_universe_discovery_packet(discovery_pkt_1);
    let res2: bool = dmx_rcv.handle_universe_discovery_packet(discovery_pkt_2);

    assert!(!res1); // Should be false because first packet isn't complete as its only the first page.
    assert!(res2); // Should be true because the second and last page is now received.

    assert_eq!(dmx_rcv.discovered_sources.len(), 1);

    assert_eq!(dmx_rcv.discovered_sources[0].name, name);
    assert_eq!(dmx_rcv.discovered_sources[0].last_page, last_page);
    assert_eq!(dmx_rcv.discovered_sources[0].pages.len(), 2);
    assert_eq!(dmx_rcv.discovered_sources[0].pages[0].page, 0);
    assert_eq!(dmx_rcv.discovered_sources[0].pages[1].page, 1);
    assert_eq!(
        dmx_rcv.discovered_sources[0].pages[0].universes,
        universes_page_1
    );
    assert_eq!(
        dmx_rcv.discovered_sources[0].pages[1].universes,
        universes_page_2
    );
}

#[test]
fn test_store_retrieve_waiting_data() {
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), ACN_SDT_MULTICAST_PORT);

    let mut dmx_rcv = SacnReceiver::with_ip(addr, None).unwrap();

    let sync_uni: u16 = 1;
    let universe: u16 = 1;
    let vals: Vec<u8> = vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 9];

    let dmx_data = DMXData {
        universe: universe,
        values: vals.clone(),
        sync_uni: sync_uni,
        priority: 100,
        src_cid: None,
        preview: false,
    };

    dmx_rcv.store_waiting_data(dmx_data).unwrap();

    let res: Vec<DMXData> = dmx_rcv.rtrv_waiting_data(sync_uni);

    assert_eq!(res.len(), 1);
    assert_eq!(res[0].universe, universe);
    assert_eq!(res[0].sync_uni, sync_uni);
    assert_eq!(res[0].values, vals);
}

#[test]
fn test_store_2_retrieve_1_waiting_data() {
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), ACN_SDT_MULTICAST_PORT);

    let mut dmx_rcv = SacnReceiver::with_ip(addr, None).unwrap();

    let sync_uni: u16 = 1;
    let universe: u16 = 1;
    let vals: Vec<u8> = vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 9];

    let dmx_data = DMXData {
        universe: universe,
        values: vals.clone(),
        sync_uni: sync_uni,
        priority: 100,
        src_cid: None,
        preview: false,
    };

    let dmx_data2 = DMXData {
        universe: universe + 1,
        values: vals.clone(),
        sync_uni: sync_uni + 1,
        priority: 100,
        src_cid: None,
        preview: false,
    };

    dmx_rcv.store_waiting_data(dmx_data).unwrap();
    dmx_rcv.store_waiting_data(dmx_data2).unwrap();

    let res: Vec<DMXData> = dmx_rcv.rtrv_waiting_data(sync_uni);

    assert_eq!(res.len(), 1);
    assert_eq!(res[0].universe, universe);
    assert_eq!(res[0].sync_uni, sync_uni);
    assert_eq!(res[0].values, vals);
}

#[test]
fn test_store_2_retrieve_2_waiting_data() {
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), ACN_SDT_MULTICAST_PORT);

    let mut dmx_rcv = SacnReceiver::with_ip(addr, None).unwrap();

    let sync_uni: u16 = 1;
    let universe: u16 = 1;
    let vals: Vec<u8> = vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 9];

    let dmx_data = DMXData {
        universe: universe,
        values: vals.clone(),
        sync_uni: sync_uni,
        priority: 100,
        src_cid: None,
        preview: false,
    };

    let vals2: Vec<u8> = vec![0, 9, 7, 3, 2, 4, 5, 6, 5, 1, 2, 3];

    let dmx_data2 = DMXData {
        universe: universe + 1,
        values: vals2.clone(),
        sync_uni: sync_uni + 1,
        priority: 100,
        src_cid: None,
        preview: false,
    };

    dmx_rcv.store_waiting_data(dmx_data).unwrap();
    dmx_rcv.store_waiting_data(dmx_data2).unwrap();

    let res: Vec<DMXData> = dmx_rcv.rtrv_waiting_data(sync_uni);

    assert_eq!(res.len(), 1);
    assert_eq!(res[0].universe, universe);
    assert_eq!(res[0].sync_uni, sync_uni);
    assert_eq!(res[0].values, vals);

    let res2: Vec<DMXData> = dmx_rcv.rtrv_waiting_data(sync_uni + 1);

    assert_eq!(res2.len(), 1);
    assert_eq!(res2[0].universe, universe + 1);
    assert_eq!(res2[0].sync_uni, sync_uni + 1);
    assert_eq!(res2[0].values, vals2);
}

#[test]
fn test_store_2_same_universe_same_priority_waiting_data() {
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), ACN_SDT_MULTICAST_PORT);

    let mut dmx_rcv = SacnReceiver::with_ip(addr, None).unwrap();

    let sync_uni: u16 = 1;
    let universe: u16 = 1;
    let vals: Vec<u8> = vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 9];

    let dmx_data = DMXData {
        universe: universe,
        values: vals.clone(),
        sync_uni: sync_uni,
        priority: 100,
        src_cid: None,
        preview: false,
    };

    let vals2: Vec<u8> = vec![0, 9, 7, 3, 2, 4, 5, 6, 5, 1, 2, 3];

    let dmx_data2 = DMXData {
        universe: universe,
        values: vals2.clone(),
        sync_uni: sync_uni,
        priority: 100,
        src_cid: None,
        preview: false,
    };

    dmx_rcv.store_waiting_data(dmx_data).unwrap();
    dmx_rcv.store_waiting_data(dmx_data2).unwrap();

    let res2: Vec<DMXData> = dmx_rcv.rtrv_waiting_data(sync_uni);

    assert_eq!(res2.len(), 1);
    assert_eq!(res2[0].universe, universe);
    assert_eq!(res2[0].sync_uni, sync_uni);
    assert_eq!(res2[0].values, vals2);

    assert_eq!(dmx_rcv.rtrv_waiting_data(sync_uni).len(), 0);
}

#[test]
fn test_store_2_same_universe_diff_priority_waiting_data() {
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), ACN_SDT_MULTICAST_PORT);

    let mut dmx_rcv = SacnReceiver::with_ip(addr, None).unwrap();

    let sync_uni: u16 = 1;
    let universe: u16 = 1;
    let vals: Vec<u8> = vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 9];

    let dmx_data = DMXData {
        universe: universe,
        values: vals.clone(),
        sync_uni: sync_uni,
        priority: 120,
        src_cid: None,
        preview: false,
    };

    let vals2: Vec<u8> = vec![0, 9, 7, 3, 2, 4, 5, 6, 5, 1, 2, 3];

    let dmx_data2 = DMXData {
        universe: universe,
        values: vals2.clone(),
        sync_uni: sync_uni,
        priority: 100,
        src_cid: None,
        preview: false,
    };

    dmx_rcv.store_waiting_data(dmx_data).unwrap();
    dmx_rcv.store_waiting_data(dmx_data2).unwrap(); // Won't be added as lower priority than already waiting data.

    let res: Vec<DMXData> = dmx_rcv.rtrv_waiting_data(sync_uni);

    assert_eq!(res.len(), 1);
    assert_eq!(res[0].universe, universe);
    assert_eq!(res[0].sync_uni, sync_uni);
    assert_eq!(res[0].values, vals);

    assert_eq!(dmx_rcv.rtrv_waiting_data(sync_uni).len(), 0);
}

