use crate::dtls::{dtls_ssl_create, DtlsEvent, DtlsRx, DtlsStream, DtlsTx};
use crate::error::Error;
use crate::media::Media;
use crate::peer::{Peer, PeerInput, PeerUdp};
use crate::rt::{mpsc, spawn, AsyncReadExt};
use crate::rtcp;
use crate::rtp;
use crate::sdp::Sdp;
use crate::sdp::{MediaAttributeThings, StreamId};
use crate::server::{BufExt, ServerOut, UdpKind};
use crate::srtp::SrtpContext;
use crate::util::Ts;
use openssl::ssl::SslContext;
use std::net::SocketAddr;

/// WebRTC session for a Peer.
///
/// There can only be one of these per Peer. Each RTCSession will typically have only one
/// RtcConnection, but multiple are possible when a peer changes IP mid session.
#[derive(Debug)]
pub struct RtcSession {
    /// DTLS context for entire session.
    dtls_ctx: SslContext,

    /// Mapping of RTP extension id to extension type for the entire session.
    /// Our assumption is that a specific extension type cannot have two different
    /// mappings within the same session.
    id_to_ext: rtp::IdToExtType,

    conns: Vec<RtcConnection>,

    tx_server: ServerOut,
}

/// Holds state for one single SocketAddr beloning to a RtcSession.
#[derive(Debug)]
pub struct RtcConnection {
    remote_addr: SocketAddr,
    tx_dtls: DtlsTx,
    // srtp input context
    srtp_rx: Option<SrtpContext>,
    // srtp output context
    srtp_tx: Option<SrtpContext>,
}

impl RtcSession {
    pub fn new(dtls_ctx: SslContext, tx_server: ServerOut) -> Self {
        RtcSession {
            dtls_ctx,
            id_to_ext: rtp::IdToExtType::new(),
            conns: vec![],
            tx_server,
        }
    }

    pub fn update_from_sdp(&mut self, sdp: &Sdp) -> Result<(), Error> {
        for m in &sdp.media {
            let extmaps = m.attrs.extmaps();
            self.id_to_ext.apply_ext_map(&extmaps)?;
        }
        Ok(())
    }

    pub async fn handle_udp(&mut self, peer: &mut Peer, udp: PeerUdp) -> Option<()> {
        // Local copy to not borrow mut and not from self.
        let id_to_ext = self.id_to_ext.clone();

        // The connection this UDP packet is for.
        let conn = match self.connection_by_remote_addr(&udp.addr) {
            // Reuse existing connection.
            Some(conn) => conn,
            // Packets are already verified to be for this Peer by virtue of
            // the STUN authentication in server.rs
            None => self.create_connection(&udp.addr, peer.tx.clone()),
        };

        match udp.buf.udp_kind() {
            UdpKind::Dtls => {
                conn.tx_dtls.send(udp.buf).await;
            }

            UdpKind::Rtp => {
                handle_rtp(peer, udp, &id_to_ext, conn);
            }

            UdpKind::Rtcp => {
                handle_rtcp(peer, udp, conn);
            }

            _ => debug!("Unexpected PeerUdp kind: {:?}", udp.buf.udp_kind()),
        }
        Some(())
    }

    pub fn create_connection(
        &mut self,
        addr: &SocketAddr,
        tx_peer: mpsc::Sender<PeerInput>,
    ) -> &mut RtcConnection {
        let ssl = dtls_ssl_create(&self.dtls_ctx).expect("dtls_ssl_create");

        let parts = DtlsStream::accept(addr.clone(), ssl);
        let (tx_dtls, rx_dtls, rx_event, mut dtls) = parts;

        let mut sender = DtlsSender(rx_dtls, self.tx_server.clone(), addr.clone());
        spawn(async move {
            sender.handle().await;
        });

        let mut eventer = DtlsEventer(rx_event, tx_peer);
        spawn(async move {
            eventer.handle().await;
        });

        // TODO this is temporary until we do SCTP
        spawn(async move {
            loop {
                let mut buf = [0_u8; 10];
                match dtls.read(&mut buf[..]).await {
                    Ok(v) => {
                        info!("DTLS data: {}", buf.len());
                        if v == 0 {
                            break;
                        }
                    }
                    Err(e) => {
                        // expected when we shut down peer
                        trace!("DTLS data error: {:?}", e);
                        break;
                    }
                }
            }
        });

        let conn = RtcConnection {
            remote_addr: addr.clone(),
            tx_dtls,
            srtp_rx: None,
            srtp_tx: None,
        };
        let last = self.conns.len();
        self.conns.push(conn);
        &mut self.conns[last]
    }

    /// Find a connection using the remote socket address.
    pub fn connection_by_remote_addr(&mut self, addr: &SocketAddr) -> Option<&mut RtcConnection> {
        for conn in &mut self.conns {
            if conn.remote_addr == *addr {
                return Some(conn);
            }
        }
        None
    }
}

fn handle_rtp(
    peer: &mut Peer,
    udp: PeerUdp,
    id_to_ext: &rtp::IdToExtType,
    conn: &mut RtcConnection,
) -> Option<()> {
    //

    let header = rtp::parse_header(&udp.buf, &id_to_ext)?;
    let ssrc = header.ssrc;

    // info!("RTP: {:?}", header);

    // Only exists if DTLS is established.
    let srtp_ctx = conn.srtp_rx.as_mut()?;

    // Try different strategies for matching the RTP to a Media.
    // This might create ingress streams to handle the RTP.
    let media = if let Some(media) = peer.media_by_ingress_ssrc(ssrc) {
        // Direct match. Once we established an SSRC for a peer, we
        // prefer this direct association over using the mid.
        Some(media)
    } else if let Some(media) = header.ext.rtp_mid.and_then(|mid| peer.media_by_mid(mid)) {
        // Match via mid. Associate SSRC to this mid.
        debug!("Associate SSRC {} with {:?}", ssrc, media.media_id);

        media.ingress_create_ssrc(ssrc);

        Some(media)
    } else {
        None
    };

    let media = media?;
    let media_ptr = media as *mut Media;

    // If we don't have an ingress stream by now, we failed to
    // match the RTP to any incoming Media.
    let stream = media.ingress_by_ssrc(ssrc)?;

    // Repair streams have both "mid" and "rep_stream_id". They are
    // associated to Media via mid and this updates the repaired_ssrc
    // reference.
    if header.ext.rep_stream_id.is_some() && stream.repaired_ssrc.is_none() {
        let stream_id = header.ext.rep_stream_id.unwrap();

        let repaired_ssrc = unsafe {
            // We need to know the SSRC value of another IngressStream
            // in the same Media, however we got media borrowed already.
            // This unsafe is ok because we're only reading a value of
            // an unrelated object.
            let repaired_stream = media_ptr.as_mut().unwrap().ingress_by_stream_id(stream_id);
            repaired_stream.map(|s| s.ssrc)
        };

        if let Some(repaired_ssrc) = repaired_ssrc {
            // Mark this stream as being a repair stream
            stream.repaired_ssrc = Some(repaired_ssrc);

            debug!("SSRC {} repairs {}", ssrc, repaired_ssrc);
        }
    }

    if let Some(stream_id) = header.ext.stream_id {
        // There might be a SDES stream_id for this RTP stream. We associate that
        // first time we discover the association.
        if stream.stream_id.is_none() {
            let stream_id = StreamId(stream_id.to_string());
            debug!("Associate SSRC {} with {:?}", ssrc, stream_id);
            stream.stream_id = Some(stream_id);
        }
    }

    // Lookup the corresponding format for the incoming Ingress.
    // This must be done after determining whether the stream is a repair stream, since
    // that fact will impact which format line to choose.
    let format = unsafe { media_ptr.as_mut().unwrap().format_for_ingress(&stream) }?;
    let rtp_time = Ts::new(header.timestamp, format.clock_rate);

    let ext_seq = rtp::extend_seq(stream.rtp_ext_seq, header.sequence_number);

    if let Some(p) = stream.rtp_ext_seq {
        // should ever only increase
        if ext_seq > p {
            stream.rtp_ext_seq = Some(ext_seq);
        }
    }

    let decrypted = srtp_ctx.unprotect_rtp(&udp.buf, &header, ext_seq)?;

    if stream.rtp_packet_count == 0 {
        // First even sequence we see for this RTP stream.
        stream.rtp_start_seq = ext_seq;
    }
    stream.rtp_max_seq = ext_seq.max(stream.rtp_max_seq);

    stream.rtp_packet_count += 1;
    stream.rtp_bytes += decrypted.len() as u64;

    stream.estimate_jitter(udp.timestamp, rtp_time);

    info!("RTP: {:?} {:?} {:02X?}", header, format, &decrypted[0..10]);

    Some(())
}

fn handle_rtcp(peer: &mut Peer, udp: PeerUdp, conn: &mut RtcConnection) -> Option<()> {
    // parse header to verify the first (unprotected) header is valid.
    rtcp::parse_header(&udp.buf, true)?;

    // Only exists if DTLS is established.
    let srtcp_ctx = conn.srtp_rx.as_mut()?;
    let decrypted = srtcp_ctx.unprotect_rtcp(&udp.buf)?;

    // https://tools.ietf.org/html/rfc3550#section-6.1
    // Multiple RTCP packets can be concatenated without any intervening
    // separators to form a compound RTCP packet that is sent in a single
    // packet of the lower layer protocol, for example UDP.
    let mut offset = 0;
    while offset < decrypted.len() {
        let buf = &decrypted[offset..];
        let header = rtcp::parse_header(buf, false)?;
        let buf = &buf[..header.length];

        info!("RTCP: {:?} {:02X?}", header, buf);

        match header.packet_type {
            rtcp::PacketType::SenderReport => {
                rtcp::handle_sender_report(&udp, peer, buf);
            }
            rtcp::PacketType::ReceiverReport => {
                //
            }
            rtcp::PacketType::SourceDescription => {
                //
            }
            rtcp::PacketType::Goodbye => {
                //
            }
            rtcp::PacketType::ApplicationDefined => {
                //
            }
            rtcp::PacketType::TransportLayerFeedback => {
                //
            }
            rtcp::PacketType::PayloadSpecificFeedback => {
                //
            }
            rtcp::PacketType::ExtendedReport => {
                //
            }
        }

        offset += header.length;
    }

    Some(())
}

impl RtcConnection {
    pub fn set_srtp_context(&mut self, srtp_rx: SrtpContext, srtp_tx: SrtpContext) {
        self.srtp_rx = Some(srtp_rx);
        self.srtp_tx = Some(srtp_tx);
    }
}

/// Forwarder of DtlsStream input to Server UDP output.
struct DtlsSender(DtlsRx, ServerOut, SocketAddr);
impl DtlsSender {
    async fn handle(&mut self) {
        loop {
            if let Some(buf) = self.0.recv().await {
                self.1.udp.send((buf, self.2.clone())).await.ok();
            } else {
                trace!("DtlsSender end");
                break;
            }
        }
    }
}

/// Forwarder of DtlsEvents from DtlsStream to _Peer_.
///
/// The main thing we want out of DtlsEvent::Connected is "keying material", which is
/// a fancy word for a master key from which we derive the keys used for SRTP and SRTCP.
/// SRTP and SRTCP is a concern for RtcPeerConnection, however…
///
/// DtlsEvents go to Peer because in the original SDP, we have received a hash of the
/// TLS certificate the remote peer is intending to use, called a "fingerprint". Once
/// DTLS is established, we must verify  the remote peer cert negotiated against the
/// hash in the SDP.
struct DtlsEventer(mpsc::Receiver<DtlsEvent>, mpsc::Sender<PeerInput>);
impl DtlsEventer {
    async fn handle(&mut self) {
        loop {
            if let Some(event) = self.0.recv().await {
                self.1.send(PeerInput::DtlsEvent(event)).await.ok();
            } else {
                trace!("DtlsEventer end");
                break;
            }
        }
    }
}