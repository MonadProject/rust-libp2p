// Copyright 2018 Parity Technologies (UK) Ltd.
//
// Permission is hereby granted, free of charge, to any person obtaining a
// copy of this software and associated documentation files (the "Software"),
// to deal in the Software without restriction, including without limitation
// the rights to use, copy, modify, merge, publish, distribute, sublicense,
// and/or sell copies of the Software, and to permit persons to whom the
// Software is furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS
// OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
// FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
// DEALINGS IN THE SOFTWARE.

use fnv::FnvHashMap;
use futures::{prelude::*, future};
use muxing::StreamMuxer;
use nodes::collection::{
    CollectionEvent, CollectionNodeAccept, CollectionReachEvent, CollectionStream, PeerMut as CollecPeerMut, ReachAttemptId,
};
use nodes::handled_node::NodeHandler;
use nodes::listeners::{ListenersEvent, ListenersStream};
use nodes::node::Substream;
use std::collections::hash_map::{Entry, OccupiedEntry};
use std::io::{Error as IoError, ErrorKind as IoErrorKind};
use {Endpoint, Multiaddr, PeerId, Transport};

/// Implementation of `Stream` that handles the nodes.
pub struct RawSwarm<TTrans, TInEvent, TOutEvent, THandler>
where
    TTrans: Transport,
{
    /// Listeners for incoming connections.
    listeners: ListenersStream<TTrans>,

    /// The nodes currently active.
    active_nodes: CollectionStream<TInEvent, TOutEvent, THandler>,

    /// The reach attempts of the swarm.
    /// This needs to be a separate struct in order to handle multiple mutable borrows issues.
    reach_attempts: ReachAttempts,
}

struct ReachAttempts {
    /// Attempts to reach a peer.
    out_reach_attempts: FnvHashMap<PeerId, OutReachAttempt>,

    /// Reach attempts for incoming connections, and outgoing connections for which we don't know
    /// the peer ID.
    other_reach_attempts: Vec<(ReachAttemptId, ConnectedPoint)>,

    /// For each peer ID we're connected to, contains the endpoint we're connected to.
    connected_points: FnvHashMap<PeerId, ConnectedPoint>,
}

/// Attempt to reach a peer.
#[derive(Debug, Clone)]
struct OutReachAttempt {
    /// Identifier for the reach attempt.
    id: ReachAttemptId,
    /// Multiaddr currently being attempted.
    cur_attempted: Multiaddr,
    /// Multiaddresses to attempt if the current one fails.
    next_attempts: Vec<Multiaddr>,
}

/// Event that can happen on the `RawSwarm`.
pub enum RawSwarmEvent<'a, TTrans: 'a, TInEvent: 'a, TOutEvent: 'a, THandler: 'a>
where
    TTrans: Transport,
{
    /// One of the listeners gracefully closed.
    ListenerClosed {
        /// Address of the listener which closed.
        listen_addr: Multiaddr,
        /// The listener which closed.
        listener: TTrans::Listener,
        /// The error that happened. `Ok` if gracefully closed.
        result: Result<(), <TTrans::Listener as Stream>::Error>,
    },

    /// A new connection arrived on a listener.
    IncomingConnection(IncomingConnectionEvent<'a, TTrans, TInEvent, TOutEvent, THandler>),

    /// A new connection was arriving on a listener, but an error happened when negotiating it.
    ///
    /// This can include, for example, an error during the handshake of the encryption layer, or
    /// the connection unexpectedly closed.
    IncomingConnectionError {
        /// Address of the listener which received the connection.
        listen_addr: Multiaddr,
        /// Address used to send back data to the remote.
        send_back_addr: Multiaddr,
        /// The error that happened.
        error: IoError,
    },

    /// A new connection to a peer has been opened.
    Connected {
        /// Id of the peer.
        peer_id: PeerId,
        /// If `Listener`, then we received the connection. If `Dial`, then it's a connection that
        /// we opened.
        endpoint: ConnectedPoint,
    },

    /// A connection to a peer has been replaced with a new one.
    Replaced {
        /// Id of the peer.
        peer_id: PeerId,
        /// Endpoint we were connected to.
        closed_endpoint: ConnectedPoint,
        /// If `Listener`, then we received the connection. If `Dial`, then it's a connection that
        /// we opened.
        endpoint: ConnectedPoint,
    },

    /// A connection to a node has been closed.
    ///
    /// This happens once both the inbound and outbound channels are closed, and no more outbound
    /// substream attempt is pending.
    NodeClosed {
        /// Identifier of the node.
        peer_id: PeerId,
        /// Endpoint we were connected to.
        endpoint: ConnectedPoint,
    },

    /// The muxer of a node has produced an error.
    NodeError {
        /// Identifier of the node.
        peer_id: PeerId,
        /// Endpoint we were connected to.
        endpoint: ConnectedPoint,
        /// The error that happened.
        error: IoError,
    },

    /// Failed to reach a peer that we were trying to dial.
    DialError {
        /// Returns the number of multiaddresses that still need to be attempted. If this is
        /// non-zero, then there's still a chance we can connect to this node. If this is zero,
        /// then we have definitely failed.
        remain_addrs_attempt: usize,

        /// Id of the peer we were trying to dial.
        peer_id: PeerId,

        /// The multiaddr we failed to reach.
        multiaddr: Multiaddr,

        /// The error that happened.
        error: IoError,
    },

    /// Failed to reach a peer that we were trying to dial.
    UnknownPeerDialError {
        /// The multiaddr we failed to reach.
        multiaddr: Multiaddr,

        /// The error that happened.
        error: IoError,

        /// The handler that was passed to `dial()`.
        handler: THandler,
    },

    /// A node produced a custom event.
    NodeEvent {
        /// Id of the node that produced the event.
        peer_id: PeerId,
        /// Event that was produced by the node.
        event: TOutEvent,
    },
}

/// A new connection arrived on a listener.
pub struct IncomingConnectionEvent<'a, TTrans: 'a, TInEvent: 'a, TOutEvent: 'a, THandler: 'a>
where TTrans: Transport
{
    /// The produced upgrade.
    upgrade: TTrans::ListenerUpgrade,
    /// Address of the listener which received the connection.
    listen_addr: Multiaddr,
    /// Address used to send back data to the remote.
    send_back_addr: Multiaddr,
    /// Reference to the `active_nodes` field of the swarm.
    active_nodes: &'a mut CollectionStream<TInEvent, TOutEvent, THandler>,
    /// Reference to the `other_reach_attempts` field of the swarm.
    other_reach_attempts: &'a mut Vec<(ReachAttemptId, ConnectedPoint)>,
}

impl<'a, TTrans, TInEvent, TOutEvent, TMuxer, THandler> IncomingConnectionEvent<'a, TTrans, TInEvent, TOutEvent, THandler>
where
    TTrans: Transport<Output = (PeerId, TMuxer)> + Clone,
    TTrans::Dial: Send + 'static,
    TTrans::ListenerUpgrade: Send + 'static,
    THandler: NodeHandler<Substream = Substream<TMuxer>, InEvent = TInEvent, OutEvent = TOutEvent> + Send + 'static,
    THandler::OutboundOpenInfo: Send + 'static, // TODO: shouldn't be necessary
    TMuxer: StreamMuxer + Send + Sync + 'static,
    TMuxer::OutboundSubstream: Send,
    TMuxer::Substream: Send,
    TInEvent: Send + 'static,
    TOutEvent: Send + 'static,
{
    /// Starts processing the incoming connection and sets the handler to use for it.
    #[inline]
    pub fn accept(self, handler: THandler) {
        self.accept_with_builder(|_| handler)
    }

    /// Same as `accept`, but accepts a closure that turns a `ConnectedPoint` into a handler.
    pub fn accept_with_builder<TBuilder>(self, builder: TBuilder)
    where TBuilder: FnOnce(&ConnectedPoint) -> THandler
    {
        let connected_point = self.to_connected_point();
        let handler = builder(&connected_point);
        let id = self.active_nodes.add_reach_attempt(self.upgrade, handler);
        self.other_reach_attempts.push((
            id,
            connected_point,
        ));
    }
}

impl<'a, TTrans, TInEvent, TOutEvent, THandler> IncomingConnectionEvent<'a, TTrans, TInEvent, TOutEvent, THandler>
where TTrans: Transport
{
    /// Address of the listener that received the connection.
    #[inline]
    pub fn listen_addr(&self) -> &Multiaddr {
        &self.listen_addr
    }

    /// Address used to send back data to the dialer.
    #[inline]
    pub fn send_back_addr(&self) -> &Multiaddr {
        &self.send_back_addr
    }

    /// Builds the `ConnectedPoint` corresponding to the incoming connection.
    #[inline]
    pub fn to_connected_point(&self) -> ConnectedPoint {
        ConnectedPoint::Listener {
            listen_addr: self.listen_addr.clone(),
            send_back_addr: self.send_back_addr.clone(),
        }
    }
}

/// How we connected to a node.
// TODO: move definition
#[derive(Debug, Clone)]
pub enum ConnectedPoint {
    /// We dialed the node.
    Dialer {
        /// Multiaddress that was successfully dialed.
        address: Multiaddr,
    },
    /// We received the node.
    Listener {
        /// Address of the listener that received the connection.
        listen_addr: Multiaddr,
        /// Stack of protocols used to send back data to the remote.
        send_back_addr: Multiaddr,
    },
}

impl<'a> From<&'a ConnectedPoint> for Endpoint {
    #[inline]
    fn from(endpoint: &'a ConnectedPoint) -> Endpoint {
        endpoint.to_endpoint()
    }
}

impl From<ConnectedPoint> for Endpoint {
    #[inline]
    fn from(endpoint: ConnectedPoint) -> Endpoint {
        endpoint.to_endpoint()
    }
}

impl ConnectedPoint {
    /// Turns the `ConnectedPoint` into the corresponding `Endpoint`.
    #[inline]
    pub fn to_endpoint(&self) -> Endpoint {
        match *self {
            ConnectedPoint::Dialer { .. } => Endpoint::Dialer,
            ConnectedPoint::Listener { .. } => Endpoint::Listener,
        }
    }

    /// Returns true if we are `Dialer`.
    #[inline]
    pub fn is_dialer(&self) -> bool {
        match *self {
            ConnectedPoint::Dialer { .. } => true,
            ConnectedPoint::Listener { .. } => false,
        }
    }

    /// Returns true if we are `Listener`.
    #[inline]
    pub fn is_listener(&self) -> bool {
        match *self {
            ConnectedPoint::Dialer { .. } => false,
            ConnectedPoint::Listener { .. } => true,
        }
    }
}

impl<TTrans, TInEvent, TOutEvent, TMuxer, THandler>
    RawSwarm<TTrans, TInEvent, TOutEvent, THandler>
where
    TTrans: Transport<Output = (PeerId, TMuxer)> + Clone,
    TMuxer: StreamMuxer,
    THandler: NodeHandler<Substream = Substream<TMuxer>, InEvent = TInEvent, OutEvent = TOutEvent> + Send + 'static,
    THandler::OutboundOpenInfo: Send + 'static, // TODO: shouldn't be necessary
{
    /// Creates a new node events stream.
    #[inline]
    pub fn new(transport: TTrans) -> Self {
        // TODO: with_capacity?
        RawSwarm {
            listeners: ListenersStream::new(transport),
            active_nodes: CollectionStream::new(),
            reach_attempts: ReachAttempts {
                out_reach_attempts: Default::default(),
                other_reach_attempts: Vec::new(),
                connected_points: Default::default(),
            },
        }
    }

    /// Returns the transport passed when building this object.
    #[inline]
    pub fn transport(&self) -> &TTrans {
        self.listeners.transport()
    }

    /// Start listening on the given multiaddress.
    #[inline]
    pub fn listen_on(&mut self, addr: Multiaddr) -> Result<Multiaddr, Multiaddr> {
        self.listeners.listen_on(addr)
    }

    /// Returns an iterator that produces the list of addresses we're listening on.
    #[inline]
    pub fn listeners(&self) -> impl Iterator<Item = &Multiaddr> {
        self.listeners.listeners()
    }

    /// Call this function in order to know which address remotes should dial in order to access
    /// your local node.
    ///
    /// `observed_addr` should be an address a remote observes you as, which can be obtained for
    /// example with the identify protocol.
    ///
    /// For each listener, calls `nat_traversal` with the observed address and returns the outcome.
    #[inline]
    pub fn nat_traversal<'a>(
        &'a self,
        observed_addr: &'a Multiaddr,
    ) -> impl Iterator<Item = Multiaddr> + 'a
        where TMuxer: 'a,
              THandler: 'a,
    {
        self.listeners()
            .flat_map(move |server| self.transport().nat_traversal(server, observed_addr))
    }

    /// Dials a multiaddress without knowing the peer ID we're going to obtain.
    ///
    /// The second parameter is the handler to use if we manage to reach a node.
    pub fn dial(&mut self, addr: Multiaddr, handler: THandler) -> Result<(), Multiaddr>
    where
        TTrans: Transport<Output = (PeerId, TMuxer)> + Clone,
        TTrans::Dial: Send + 'static,
        TMuxer: StreamMuxer + Send + Sync + 'static,
        TMuxer::OutboundSubstream: Send,
        TMuxer::Substream: Send,
        TInEvent: Send + 'static,
        TOutEvent: Send + 'static,
    {
        let future = match self.transport().clone().dial(addr.clone()) {
            Ok(fut) => fut,
            Err((_, addr)) => return Err(addr),
        };

        let connected_point = ConnectedPoint::Dialer { address: addr };
        let reach_id = self.active_nodes.add_reach_attempt(future, handler);
        self.reach_attempts.other_reach_attempts.push((reach_id, connected_point));
        Ok(())
    }

    /// Returns the number of incoming connections that are currently in the process of being
    /// negotiated.
    ///
    /// We don't know anything about these connections yet, so all we can do is know how many of
    /// them we have.
    // TODO: thats's not true as we should be able to know their multiaddress, but that requires
    // a lot of API changes
    #[inline]
    pub fn num_incoming_negotiated(&self) -> usize {
        self.reach_attempts.other_reach_attempts
            .iter()
            .filter(|&(_, endpoint)| endpoint.is_listener())
            .count()
    }

    /// Sends an event to all nodes.
    #[inline]
    pub fn broadcast_event(&mut self, event: &TInEvent)
    where TInEvent: Clone,
    {
        self.active_nodes.broadcast_event(event)
    }

    /// Grants access to a struct that represents a peer.
    #[inline]
    pub fn peer(&mut self, peer_id: PeerId) -> Peer<TTrans, TInEvent, TOutEvent, THandler> {
        // TODO: we do `peer_mut(...).is_some()` followed with `peer_mut(...).unwrap()`, otherwise
        // the borrow checker yells at us.

        if self.active_nodes.peer_mut(&peer_id).is_some() {
            debug_assert!(!self.reach_attempts.out_reach_attempts.contains_key(&peer_id));
            return Peer::Connected(PeerConnected {
                peer: self
                    .active_nodes
                    .peer_mut(&peer_id)
                    .expect("we checked for Some just above"),
                peer_id,
                connected_points: &mut self.reach_attempts.connected_points,
            });
        }

        if self.reach_attempts.out_reach_attempts.get_mut(&peer_id).is_some() {
            debug_assert!(!self.reach_attempts.connected_points.contains_key(&peer_id));
            return Peer::PendingConnect(PeerPendingConnect {
                attempt: match self.reach_attempts.out_reach_attempts.entry(peer_id.clone()) {
                    Entry::Occupied(e) => e,
                    Entry::Vacant(_) => panic!("we checked for Some just above"),
                },
                active_nodes: &mut self.active_nodes,
            });
        }

        debug_assert!(!self.reach_attempts.connected_points.contains_key(&peer_id));
        Peer::NotConnected(PeerNotConnected {
            nodes: self,
            peer_id,
        })
    }

    /// Starts dialing out a multiaddress. `rest` is the list of multiaddresses to attempt if
    /// `first` fails.
    ///
    /// It is a logic error to call this method if we already have an outgoing attempt to the
    /// given peer.
    fn start_dial_out(&mut self, peer_id: PeerId, handler: THandler, first: Multiaddr,
                      rest: Vec<Multiaddr>)
    where
        TTrans: Transport<Output = (PeerId, TMuxer)> + Clone,
        TTrans::Dial: Send + 'static,
        TMuxer: StreamMuxer + Send + Sync + 'static,
        TMuxer::OutboundSubstream: Send,
        TMuxer::Substream: Send,
        TInEvent: Send + 'static,
        TOutEvent: Send + 'static,
    {
        let reach_id = match self.transport().clone().dial(first.clone()) {
            Ok(fut) => {
                let expected_peer_id = peer_id.clone();
                let fut = fut.and_then(move |(actual_peer_id, muxer)| {
                    if actual_peer_id == expected_peer_id {
                        Ok((actual_peer_id, muxer))
                    } else {
                        let msg = format!("public key mismatch; expected = {:?}; obtained = {:?}",
                                          expected_peer_id, actual_peer_id);
                        Err(IoError::new(IoErrorKind::Other, msg))
                    }
                });
                self.active_nodes.add_reach_attempt(fut, handler)
            },
            Err((_, addr)) => {
                let msg = format!("unsupported multiaddr {}", addr);
                let fut = future::err(IoError::new(IoErrorKind::Other, msg));
                self.active_nodes.add_reach_attempt(fut, handler)
            },
        };

        let former = self.reach_attempts.out_reach_attempts.insert(
            peer_id,
            OutReachAttempt {
                id: reach_id,
                cur_attempted: first,
                next_attempts: rest,
            },
        );

        debug_assert!(former.is_none());
    }

    /// Provides an API similar to `Stream`, except that it cannot error.
    pub fn poll(&mut self) -> Async<RawSwarmEvent<TTrans, TInEvent, TOutEvent, THandler>>
    where
        TTrans: Transport<Output = (PeerId, TMuxer)> + Clone,
        TTrans::Dial: Send + 'static,
        TTrans::ListenerUpgrade: Send + 'static,
        TMuxer: StreamMuxer + Send + Sync + 'static,
        TMuxer::OutboundSubstream: Send,
        TMuxer::Substream: Send,
        TInEvent: Send + 'static,
        TOutEvent: Send + 'static,
        THandler: NodeHandler<Substream = Substream<TMuxer>, InEvent = TInEvent, OutEvent = TOutEvent> + Send + 'static,
        THandler::OutboundOpenInfo: Send + 'static, // TODO: shouldn't be necessary
    {
        // Start by polling the listeners for events.
        match self.listeners.poll() {
            Async::NotReady => (),
            Async::Ready(ListenersEvent::Incoming {
                upgrade,
                listen_addr,
                send_back_addr,
            }) => {
                let event = IncomingConnectionEvent {
                    upgrade,
                    listen_addr,
                    send_back_addr,
                    active_nodes: &mut self.active_nodes,
                    other_reach_attempts: &mut self.reach_attempts.other_reach_attempts,
                };

                return Async::Ready(RawSwarmEvent::IncomingConnection(event));
            }
            Async::Ready(ListenersEvent::Closed {
                listen_addr,
                listener,
                result,
            }) => {
                return Async::Ready(RawSwarmEvent::ListenerClosed {
                    listen_addr,
                    listener,
                    result,
                });
            }
        }

        // Poll the existing nodes.
        loop {
            let (action, out_event);
            match self.active_nodes.poll() {
                Async::NotReady => break,
                Async::Ready(CollectionEvent::NodeReached(reach_event)) => {
                    let (a, e) = handle_node_reached(&mut self.reach_attempts, reach_event);
                    action = a;
                    out_event = e;
                }
                Async::Ready(CollectionEvent::ReachError { id, error, handler }) => {
                    let (a, e) = handle_reach_error(&mut self.reach_attempts, id, error, handler);
                    action = a;
                    out_event = e;
                }
                Async::Ready(CollectionEvent::NodeError {
                    peer_id,
                    error,
                }) => {
                    let endpoint = self.reach_attempts.connected_points.remove(&peer_id)
                        .expect("We insert into connected_points whenever a connection is \
                                 opened and remove only when a connection is closed; the \
                                 underlying API is guaranteed to always deliver a connection \
                                 closed message after it has been opened, and no two closed \
                                 messages; qed");
                    debug_assert!(!self.reach_attempts.out_reach_attempts.contains_key(&peer_id));
                    action = Default::default();
                    out_event = RawSwarmEvent::NodeError {
                        peer_id,
                        endpoint,
                        error,
                    };
                }
                Async::Ready(CollectionEvent::NodeClosed { peer_id }) => {
                    let endpoint = self.reach_attempts.connected_points.remove(&peer_id)
                        .expect("We insert into connected_points whenever a connection is \
                                 opened and remove only when a connection is closed; the \
                                 underlying API is guaranteed to always deliver a connection \
                                 closed message after it has been opened, and no two closed \
                                 messages; qed");
                    debug_assert!(!self.reach_attempts.out_reach_attempts.contains_key(&peer_id));
                    action = Default::default();
                    out_event = RawSwarmEvent::NodeClosed { peer_id, endpoint };
                }
                Async::Ready(CollectionEvent::NodeEvent { peer_id, event }) => {
                    action = Default::default();
                    out_event = RawSwarmEvent::NodeEvent { peer_id, event };
                }
            };

            if let Some((peer_id, handler, first, rest)) = action.start_dial_out {
                self.start_dial_out(peer_id, handler, first, rest);
            }

            if let Some(interrupt) = action.interrupt {
                // TODO: improve proof or remove; this is too complicated right now
                self.active_nodes
                    .interrupt(interrupt)
                    .expect("interrupt is guaranteed to be gathered from `out_reach_attempts`;
                             we insert in out_reach_attempts only when we call \
                             active_nodes.add_reach_attempt, and we remove only when we call \
                             interrupt or when a reach attempt succeeds or errors; therefore the \
                             out_reach_attempts should always be in sync with the actual \
                             attempts; qed");
            }

            return Async::Ready(out_event);
        }

        Async::NotReady
    }
}

/// Internal struct indicating an action to perform of the swarm.
#[derive(Debug)]
#[must_use]
struct ActionItem<THandler> {
    start_dial_out: Option<(PeerId, THandler, Multiaddr, Vec<Multiaddr>)>,
    interrupt: Option<ReachAttemptId>,
}

impl<THandler> Default for ActionItem<THandler> {
    fn default() -> Self {
        ActionItem {
            start_dial_out: None,
            interrupt: None,
        }
    }
}

/// Handles a node reached event from the collection.
///
/// Returns an event to return from the stream.
///
/// > **Note**: The event **must** have been produced by the collection of nodes, otherwise
/// >           panics will likely happen.
fn handle_node_reached<'a, TTrans, TMuxer, TInEvent, TOutEvent, THandler>(
    reach_attempts: &mut ReachAttempts,
    event: CollectionReachEvent<TInEvent, TOutEvent, THandler>
) -> (ActionItem<THandler>, RawSwarmEvent<'a, TTrans, TInEvent, TOutEvent, THandler>)
where
    TTrans: Transport<Output = (PeerId, TMuxer)> + Clone,
    TTrans::Dial: Send + 'static,
    TMuxer: StreamMuxer + Send + Sync + 'static,
    TMuxer::OutboundSubstream: Send,
    TMuxer::Substream: Send,
    TInEvent: Send + 'static,
    TOutEvent: Send + 'static,
{
    // We first start looking in the incoming attempts. While this makes the code less optimal,
    // it also makes the logic easier.
    if let Some(in_pos) = reach_attempts
        .other_reach_attempts
        .iter()
        .position(|i| i.0 == event.reach_attempt_id())
    {
        let (_, opened_endpoint) = reach_attempts.other_reach_attempts.swap_remove(in_pos);

        // Set the endpoint for this peer.
        let closed_endpoint = reach_attempts.connected_points.insert(event.peer_id().clone(), opened_endpoint.clone());

        // Cancel any outgoing attempt to this peer.
        let action = if let Some(attempt) = reach_attempts.out_reach_attempts.remove(&event.peer_id()) {
            debug_assert_ne!(attempt.id, event.reach_attempt_id());
            ActionItem {
                interrupt: Some(attempt.id),
                .. Default::default()
            }
        } else {
            ActionItem::default()
        };

        let (outcome, peer_id) = event.accept();
        if outcome == CollectionNodeAccept::ReplacedExisting {
            let closed_endpoint = closed_endpoint
                .expect("We insert into connected_points whenever a connection is opened and \
                         remove only when a connection is closed; the underlying API is \
                         guaranteed to always deliver a connection closed message after it has \
                         been opened, and no two closed messages; qed");
            return (action, RawSwarmEvent::Replaced {
                peer_id,
                endpoint: opened_endpoint,
                closed_endpoint,
            });
        } else {
            return (action, RawSwarmEvent::Connected { peer_id, endpoint: opened_endpoint });
        }
    }

    // Otherwise, try for outgoing attempts.
    let is_outgoing_and_ok = if let Some(attempt) = reach_attempts.out_reach_attempts.get(event.peer_id()) {
        attempt.id == event.reach_attempt_id()
    } else {
        false
    };

    // We only remove the attempt from `out_reach_attempts` if it both matches the reach id
    // and the expected peer id.
    if is_outgoing_and_ok {
        let attempt = reach_attempts.out_reach_attempts.remove(event.peer_id())
            .expect("is_outgoing_and_ok is true only if reach_attempts.out_reach_attempts.get(event.peer_id()) \
                        returned Some");

        let opened_endpoint = ConnectedPoint::Dialer {
            address: attempt.cur_attempted,
        };

        let closed_endpoint = reach_attempts.connected_points
            .insert(event.peer_id().clone(), opened_endpoint.clone());

        let (outcome, peer_id) = event.accept();
        if outcome == CollectionNodeAccept::ReplacedExisting {
            let closed_endpoint = closed_endpoint
                .expect("We insert into connected_points whenever a connection is opened and \
                        remove only when a connection is closed; the underlying API is guaranteed \
                        to always deliver a connection closed message after it has been opened, \
                        and no two closed messages; qed");
            return (Default::default(), RawSwarmEvent::Replaced {
                peer_id,
                endpoint: opened_endpoint,
                closed_endpoint,
            });
        } else {
            return (Default::default(), RawSwarmEvent::Connected { peer_id, endpoint: opened_endpoint });
        }
    }

    // We didn't find any entry in neither the outgoing connections not ingoing connections.
    // TODO: improve proof or remove; this is too complicated right now
    panic!("The API of collection guarantees that the id sent back in NodeReached (which is where \
            we call handle_node_reached) is one that was passed to add_reach_attempt. Whenever we \
            call add_reach_attempt, we also insert at the same time an entry either in \
            out_reach_attempts or in other_reach_attempts. It is therefore guaranteed that we \
            find back this ID in either of these two sets");
}

/// Handles a reach error event from the collection.
///
/// Optionally returns an event to return from the stream.
///
/// > **Note**: The event **must** have been produced by the collection of nodes, otherwise
/// >           panics will likely happen.
fn handle_reach_error<'a, TTrans, TInEvent, TOutEvent, THandler>(
    reach_attempts: &mut ReachAttempts,
    reach_id: ReachAttemptId,
    error: IoError,
    handler: THandler,
) -> (ActionItem<THandler>, RawSwarmEvent<'a, TTrans, TInEvent, TOutEvent, THandler>)
where TTrans: Transport
{
    // Search for the attempt in `out_reach_attempts`.
    // TODO: could be more optimal than iterating over everything
    let out_reach_peer_id = reach_attempts
        .out_reach_attempts
        .iter()
        .find(|(_, a)| a.id == reach_id)
        .map(|(p, _)| p.clone());
    if let Some(peer_id) = out_reach_peer_id {
        let mut attempt = reach_attempts.out_reach_attempts.remove(&peer_id)
            .expect("out_reach_peer_id is a key that is grabbed from out_reach_attempts");

        let num_remain = attempt.next_attempts.len();
        let failed_addr = attempt.cur_attempted.clone();

        let action = if !attempt.next_attempts.is_empty() {
            let mut attempt = attempt;
            let next_attempt = attempt.next_attempts.remove(0);
            ActionItem {
                start_dial_out: Some((peer_id.clone(), handler, next_attempt, attempt.next_attempts)),
                .. Default::default()
            }
        } else {
            Default::default()
        };

        return (action, RawSwarmEvent::DialError {
            remain_addrs_attempt: num_remain,
            peer_id,
            multiaddr: failed_addr,
            error,
        });
    }

    // If this is not an outgoing reach attempt, check the incoming reach attempts.
    if let Some(in_pos) = reach_attempts
        .other_reach_attempts
        .iter()
        .position(|i| i.0 == reach_id)
    {
        let (_, endpoint) = reach_attempts.other_reach_attempts.swap_remove(in_pos);
        match endpoint {
            ConnectedPoint::Dialer { address } => {
                return (Default::default(), RawSwarmEvent::UnknownPeerDialError {
                    multiaddr: address,
                    error,
                    handler,
                });
            }
            ConnectedPoint::Listener { listen_addr, send_back_addr } => {
                return (Default::default(), RawSwarmEvent::IncomingConnectionError { listen_addr, send_back_addr, error });
            }
        }
    }

    // The id was neither in the outbound list nor the inbound list.
    // TODO: improve proof or remove; this is too complicated right now
    panic!("The API of collection guarantees that the id sent back in ReachError events \
            (which is where we call handle_reach_error) is one that was passed to \
            add_reach_attempt. Whenever we call add_reach_attempt, we also insert \
            at the same time an entry either in out_reach_attempts or in \
            other_reach_attempts. It is therefore guaranteed that we find back this ID in \
            either of these two sets");
}

/// State of a peer in the system.
pub enum Peer<'a, TTrans: 'a, TInEvent: 'a, TOutEvent: 'a, THandler: 'a>
where
    TTrans: Transport,
{
    /// We are connected to this peer.
    Connected(PeerConnected<'a, TInEvent>),

    /// We are currently attempting to connect to this peer.
    PendingConnect(PeerPendingConnect<'a, TInEvent, TOutEvent, THandler>),

    /// We are not connected to this peer at all.
    ///
    /// > **Note**: It is however possible that a pending incoming connection is being negotiated
    /// > and will connect to this peer, but we don't know it yet.
    NotConnected(PeerNotConnected<'a, TTrans, TInEvent, TOutEvent, THandler>),
}

// TODO: add other similar methods that wrap to the ones of `PeerNotConnected`
impl<'a, TTrans, TMuxer, TInEvent, TOutEvent, THandler>
    Peer<'a, TTrans, TInEvent, TOutEvent, THandler>
where
    TTrans: Transport<Output = (PeerId, TMuxer)>,
    TMuxer: StreamMuxer,
    THandler: NodeHandler<Substream = Substream<TMuxer>, InEvent = TInEvent, OutEvent = TOutEvent> + Send + 'static,
    THandler::OutboundOpenInfo: Send + 'static, // TODO: shouldn't be necessary
{
    /// If we are connected, returns the `PeerConnected`.
    #[inline]
    pub fn as_connected(self) -> Option<PeerConnected<'a, TInEvent>> {
        match self {
            Peer::Connected(peer) => Some(peer),
            _ => None,
        }
    }

    /// If a connection is pending, returns the `PeerPendingConnect`.
    #[inline]
    pub fn as_pending_connect(self) -> Option<PeerPendingConnect<'a, TInEvent, TOutEvent, THandler>> {
        match self {
            Peer::PendingConnect(peer) => Some(peer),
            _ => None,
        }
    }

    /// If we are not connected, returns the `PeerNotConnected`.
    #[inline]
    pub fn as_not_connected(self) -> Option<PeerNotConnected<'a, TTrans, TInEvent, TOutEvent, THandler>> {
        match self {
            Peer::NotConnected(peer) => Some(peer),
            _ => None,
        }
    }

    /// If we're not connected, opens a new connection to this peer using the given multiaddr.
    ///
    /// If we reach a peer but the `PeerId` doesn't correspond to the one we're expecting, then
    /// the whole connection is immediately closed.
    ///
    /// > **Note**: It is possible that the attempt reaches a node that doesn't have the peer id
    /// >           that we are expecting, in which case the handler will be used for this "wrong"
    /// >           node.
    #[inline]
    pub fn or_connect(
        self,
        addr: Multiaddr,
        handler: THandler,
    ) -> Result<PeerPotentialConnect<'a, TInEvent, TOutEvent, THandler>, Self>
    where
        TTrans: Transport<Output = (PeerId, TMuxer)> + Clone,
        TTrans::Dial: Send + 'static,
        TMuxer: StreamMuxer + Send + Sync + 'static,
        TMuxer::OutboundSubstream: Send,
        TMuxer::Substream: Send,
        TInEvent: Send + 'static,
        TOutEvent: Send + 'static,
    {
        self.or_connect_with(move |_| addr, handler)
    }

    /// If we're not connected, calls the function passed as parameter and opens a new connection
    /// using the returned address.
    ///
    /// If we reach a peer but the `PeerId` doesn't correspond to the one we're expecting, then
    /// the whole connection is immediately closed.
    ///
    /// > **Note**: It is possible that the attempt reaches a node that doesn't have the peer id
    /// >           that we are expecting, in which case the handler will be used for this "wrong"
    /// >           node.
    #[inline]
    pub fn or_connect_with<TFn>(
        self,
        addr: TFn,
        handler: THandler,
    ) -> Result<PeerPotentialConnect<'a, TInEvent, TOutEvent, THandler>, Self>
    where
        TFn: FnOnce(&PeerId) -> Multiaddr,
        TTrans: Transport<Output = (PeerId, TMuxer)> + Clone,
        TTrans::Dial: Send + 'static,
        TMuxer: StreamMuxer + Send + Sync + 'static,
        TMuxer::OutboundSubstream: Send,
        TMuxer::Substream: Send,
        TInEvent: Send + 'static,
        TOutEvent: Send + 'static,
    {
        match self {
            Peer::Connected(peer) => Ok(PeerPotentialConnect::Connected(peer)),
            Peer::PendingConnect(peer) => Ok(PeerPotentialConnect::PendingConnect(peer)),
            Peer::NotConnected(peer) => {
                let addr = addr(&peer.peer_id);
                match peer.connect(addr, handler) {
                    Ok(peer) => Ok(PeerPotentialConnect::PendingConnect(peer)),
                    Err(peer) => Err(Peer::NotConnected(peer)),
                }
            }
        }
    }
}

/// Peer we are potentially going to connect to.
pub enum PeerPotentialConnect<'a, TInEvent: 'a, TOutEvent: 'a, THandler: 'a> {
    /// We are connected to this peer.
    Connected(PeerConnected<'a, TInEvent>),

    /// We are currently attempting to connect to this peer.
    PendingConnect(PeerPendingConnect<'a, TInEvent, TOutEvent, THandler>),
}

impl<'a, TInEvent, TOutEvent, THandler> PeerPotentialConnect<'a, TInEvent, TOutEvent, THandler> {
    /// Closes the connection or the connection attempt.
    ///
    /// If the connection was active, returns the list of outbound substream openings that were
    /// closed in the process.
    // TODO: consider returning a `PeerNotConnected`
    #[inline]
    pub fn close(self) {
        match self {
            PeerPotentialConnect::Connected(peer) => peer.close(),
            PeerPotentialConnect::PendingConnect(peer) => peer.interrupt(),
        }
    }

    /// If we are connected, returns the `PeerConnected`.
    #[inline]
    pub fn as_connected(self) -> Option<PeerConnected<'a, TInEvent>> {
        match self {
            PeerPotentialConnect::Connected(peer) => Some(peer),
            _ => None,
        }
    }

    /// If a connection is pending, returns the `PeerPendingConnect`.
    #[inline]
    pub fn as_pending_connect(self) -> Option<PeerPendingConnect<'a, TInEvent, TOutEvent, THandler>> {
        match self {
            PeerPotentialConnect::PendingConnect(peer) => Some(peer),
            _ => None,
        }
    }
}

/// Access to a peer we are connected to.
pub struct PeerConnected<'a, TInEvent: 'a> {
    peer: CollecPeerMut<'a, TInEvent>,
    /// Reference to the `connected_points` field of the parent.
    connected_points: &'a mut FnvHashMap<PeerId, ConnectedPoint>,
    peer_id: PeerId,
}

impl<'a, TInEvent> PeerConnected<'a, TInEvent> {
    /// Closes the connection to this node.
    ///
    /// No `NodeClosed` message will be generated for this node.
    // TODO: consider returning a `PeerNotConnected`; however this makes all the borrows things
    // much more annoying to deal with
    pub fn close(self) {
        self.connected_points.remove(&self.peer_id);
        self.peer.close()
    }

    /// Returns the endpoint we're connected to.
    #[inline]
    pub fn endpoint(&self) -> &ConnectedPoint {
        self.connected_points.get(&self.peer_id)
            .expect("We insert into connected_points whenever a connection is opened and remove \
                     only when a connection is closed; the underlying API is guaranteed to always \
                     deliver a connection closed message after it has been opened, and no two \
                     closed messages; qed")        
    }

    /// Sends an event to the node.
    #[inline]
    pub fn send_event(&mut self, event: TInEvent) {
        self.peer.send_event(event)
    }
}

/// Access to a peer we are attempting to connect to.
pub struct PeerPendingConnect<'a, TInEvent: 'a, TOutEvent: 'a, THandler: 'a> {
    attempt: OccupiedEntry<'a, PeerId, OutReachAttempt>,
    active_nodes: &'a mut CollectionStream<TInEvent, TOutEvent, THandler>,
}

impl<'a, TInEvent, TOutEvent, THandler> PeerPendingConnect<'a, TInEvent, TOutEvent, THandler> {
    /// Interrupt this connection attempt.
    // TODO: consider returning a PeerNotConnected; however that is really pain in terms of
    // borrows
    #[inline]
    pub fn interrupt(self) {
        let attempt = self.attempt.remove();
        if self.active_nodes.interrupt(attempt.id).is_err() {
            // TODO: improve proof or remove; this is too complicated right now
            panic!("We retreived this attempt.id from out_reach_attempts. We insert in \
                    out_reach_attempts only at the same time as we call add_reach_attempt. \
                    Whenever we receive a NodeReached, NodeReplaced or ReachError event, which \
                    invalidate the attempt.id, we also remove the corresponding entry in \
                    out_reach_attempts.");
        }
    }

    /// Returns the multiaddress we're currently trying to dial.
    #[inline]
    pub fn attempted_multiaddr(&self) -> &Multiaddr {
        &self.attempt.get().cur_attempted
    }

    /// Returns a list of the multiaddresses we're going to try if the current dialing fails.
    #[inline]
    pub fn pending_multiaddrs(&self) -> impl Iterator<Item = &Multiaddr> {
        self.attempt.get().next_attempts.iter()
    }

    /// Adds a new multiaddr to attempt if the current dialing fails.
    ///
    /// Doesn't do anything if that multiaddress is already in the queue.
    pub fn append_multiaddr_attempt(&mut self, addr: Multiaddr) {
        if self.attempt.get().next_attempts.iter().any(|a| a == &addr) {
            return;
        }

        self.attempt.get_mut().next_attempts.push(addr);
    }
}

/// Access to a peer we're not connected to.
pub struct PeerNotConnected<'a, TTrans: 'a, TInEvent: 'a, TOutEvent: 'a, THandler: 'a>
where
    TTrans: Transport,
{
    peer_id: PeerId,
    nodes: &'a mut RawSwarm<TTrans, TInEvent, TOutEvent, THandler>,
}

impl<'a, TTrans, TInEvent, TOutEvent, TMuxer, THandler>
    PeerNotConnected<'a, TTrans, TInEvent, TOutEvent, THandler>
where
    TTrans: Transport<Output = (PeerId, TMuxer)>,
    TMuxer: StreamMuxer,
    THandler: NodeHandler<Substream = Substream<TMuxer>, InEvent = TInEvent, OutEvent = TOutEvent> + Send + 'static,
    THandler::OutboundOpenInfo: Send + 'static, // TODO: shouldn't be necessary
{
    /// Attempts a new connection to this node using the given multiaddress.
    ///
    /// If we reach a peer but the `PeerId` doesn't correspond to the one we're expecting, then
    /// the whole connection is immediately closed.
    #[inline]
    pub fn connect(self, addr: Multiaddr, handler: THandler) -> Result<PeerPendingConnect<'a, TInEvent, TOutEvent, THandler>, Self>
    where
        TTrans: Transport<Output = (PeerId, TMuxer)> + Clone,
        TTrans::Dial: Send + 'static,
        TMuxer: StreamMuxer + Send + Sync + 'static,
        TMuxer::OutboundSubstream: Send,
        TMuxer::Substream: Send,
        TInEvent: Send + 'static,
        TOutEvent: Send + 'static,
    {
        self.connect_inner(handler, addr, Vec::new())
    }

    /// Attempts a new connection to this node using the given multiaddresses.
    ///
    /// The multiaddresses passed as parameter will be tried one by one.
    ///
    /// If the iterator is empty, TODO: what to do? at the moment we unwrap
    ///
    /// If we reach a peer but the `PeerId` doesn't correspond to the one we're expecting, then
    /// the whole connection is immediately closed.
    #[inline]
    pub fn connect_iter<TIter>(
        self,
        addrs: TIter,
        handler: THandler,
    ) -> Result<PeerPendingConnect<'a, TInEvent, TOutEvent, THandler>, Self>
    where
        TIter: IntoIterator<Item = Multiaddr>,
        TTrans: Transport<Output = (PeerId, TMuxer)> + Clone,
        TTrans::Dial: Send + 'static,
        TMuxer: StreamMuxer + Send + Sync + 'static,
        TMuxer::OutboundSubstream: Send,
        TMuxer::Substream: Send,
        TInEvent: Send + 'static,
        TOutEvent: Send + 'static,
    {
        let mut addrs = addrs.into_iter();
        let first = addrs.next().unwrap(); // TODO: bad
        let rest = addrs.collect();
        self.connect_inner(handler, first, rest)
    }

    /// Inner implementation of `connect`.
    fn connect_inner(
        self,
        handler: THandler,
        first: Multiaddr,
        rest: Vec<Multiaddr>,
    ) -> Result<PeerPendingConnect<'a, TInEvent, TOutEvent, THandler>, Self>
    where
        TTrans: Transport<Output = (PeerId, TMuxer)> + Clone,
        TTrans::Dial: Send + 'static,
        TMuxer: StreamMuxer + Send + Sync + 'static,
        TMuxer::OutboundSubstream: Send,
        TMuxer::Substream: Send,
        TInEvent: Send + 'static,
        TOutEvent: Send + 'static,
    {
        self.nodes.start_dial_out(self.peer_id.clone(), handler, first, rest);

        Ok(PeerPendingConnect {
            attempt: match self.nodes.reach_attempts.out_reach_attempts.entry(self.peer_id) {
                Entry::Occupied(e) => e,
                Entry::Vacant(_) => {
                    panic!("We called out_reach_attempts.insert with this peer id just above")
                },
            },
            active_nodes: &mut self.nodes.active_nodes,
        })
    }
}
