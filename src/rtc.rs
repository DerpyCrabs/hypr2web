use crate::{Outgoing, Session};
use anyhow::{Context, Result};
use std::{
    sync::{Arc, atomic::Ordering},
    time::Duration,
};
use tokio::sync::mpsc;
use webrtc::{
    api::{
        APIBuilder,
        interceptor_registry::register_default_interceptors,
        media_engine::{MIME_TYPE_H264, MediaEngine},
    },
    interceptor::registry::Registry,
    media::Sample,
    peer_connection::{
        RTCPeerConnection, configuration::RTCConfiguration,
        peer_connection_state::RTCPeerConnectionState,
        sdp::session_description::RTCSessionDescription,
    },
    rtcp::payload_feedbacks::{
        full_intra_request::FullIntraRequest, picture_loss_indication::PictureLossIndication,
    },
    rtp_transceiver::{
        RTCPFeedback,
        rtp_codec::{RTCRtpCodecCapability, RTCRtpCodecParameters, RTPCodecType},
    },
    track::track_local::{TrackLocal, track_local_static_sample::TrackLocalStaticSample},
};
pub struct Rtc {
    pub peer: Arc<RTCPeerConnection>,
    pub track: Arc<TrackLocalStaticSample>,
    feedback: tokio::task::JoinHandle<()>,
}
impl Rtc {
    pub async fn new(shared: Arc<Session>, tx: mpsc::Sender<Outgoing>) -> Result<Self> {
        let mut media = MediaEngine::default();
        let codec = RTCRtpCodecCapability {
            mime_type: MIME_TYPE_H264.into(),
            clock_rate: 90000,
            sdp_fmtp_line: "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e034"
                .into(),
            rtcp_feedback: vec![
                RTCPFeedback {
                    typ: "nack".into(),
                    parameter: "".into(),
                },
                RTCPFeedback {
                    typ: "nack".into(),
                    parameter: "pli".into(),
                },
                RTCPFeedback {
                    typ: "ccm".into(),
                    parameter: "fir".into(),
                },
            ],
            ..Default::default()
        };
        media.register_codec(
            RTCRtpCodecParameters {
                capability: codec.clone(),
                payload_type: 102,
                ..Default::default()
            },
            RTPCodecType::Video,
        )?;
        let registry = register_default_interceptors(Registry::new(), &mut media)?;
        let api = APIBuilder::new()
            .with_media_engine(media)
            .with_interceptor_registry(registry)
            .build();
        // Host candidates only: LAN traffic stays local, no external STUN/TURN.
        let peer = Arc::new(api.new_peer_connection(RTCConfiguration::default()).await?);
        let track = Arc::new(TrackLocalStaticSample::new(
            codec,
            "screen".into(),
            "hypr2web".into(),
        ));
        let sender = peer
            .add_track(track.clone() as Arc<dyn TrackLocal + Send + Sync>)
            .await?;
        let state = shared.clone();
        peer.on_peer_connection_state_change(Box::new(move |s| {
            state
                .rtc_ready
                .store(s == RTCPeerConnectionState::Connected, Ordering::Relaxed);
            if s == RTCPeerConnectionState::Connected {
                state.keyframe.store(true, Ordering::Relaxed);
            }
            tracing::info!(state=%s,"WebRTC connection");
            Box::pin(async {})
        }));
        // Trickle local candidates independently of the offer/answer exchange.
        peer.on_ice_candidate(Box::new(move |candidate| {
            let tx = tx.clone();
            Box::pin(async move {
                if let Some(candidate) = candidate
                    && let Ok(candidate) = candidate.to_json()
                {
                    let _ = tx
                        .send(Outgoing::Json(
                            serde_json::json!({"type":"ice","candidate":candidate}),
                        ))
                        .await;
                }
            })
        }));
        let feedback = tokio::spawn(async move {
            while let Ok((packets, _)) = sender.read_rtcp().await {
                for p in packets {
                    if p.as_any().is::<PictureLossIndication>()
                        || p.as_any().is::<FullIntraRequest>()
                    {
                        shared.keyframe.store(true, Ordering::Relaxed);
                    }
                }
            }
        });
        Ok(Self {
            peer,
            track,
            feedback,
        })
    }
    pub async fn offer(&self) -> Result<RTCSessionDescription> {
        let offer = self.peer.create_offer(None).await?;
        self.peer.set_local_description(offer).await?;
        self.peer
            .local_description()
            .await
            .context("missing local SDP")
    }
    pub async fn close(&self) {
        self.feedback.abort();
        let _ = self.peer.close().await;
    }
}
pub async fn write(
    track: &TrackLocalStaticSample,
    data: bytes::Bytes,
    duration: Duration,
) -> Result<()> {
    track
        .write_sample(&Sample {
            data,
            duration,
            ..Default::default()
        })
        .await?;
    Ok(())
}
