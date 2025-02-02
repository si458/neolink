use anyhow::{anyhow, Context, Result};
use gstreamer::{prelude::*, Bin, Caps, Element, ElementFactory, GhostPad};
use gstreamer_app::{AppSrc, AppSrcCallbacks, AppStreamType};
use tokio::sync::mpsc::{channel as mpsc, Receiver as MpscReceiver};

use crate::{
    common::{AudFormat, StreamConfig, VidFormat},
    rtsp::gst::NeoMediaFactory,
    AnyResult,
};

pub(super) struct ClientSourceData {
    pub(super) app: AppSrc,
}

pub(super) struct ClientData {
    pub(super) vid: Option<ClientSourceData>,
    pub(super) aud: Option<ClientSourceData>,
}

pub(super) async fn make_dummy_factory(
    use_splash: bool,
    pattern: String,
) -> AnyResult<NeoMediaFactory> {
    NeoMediaFactory::new_with_callback(move |element| {
        clear_bin(&element)?;
        if !use_splash {
            Ok(None)
        } else {
            build_unknown(&element, &pattern)?;
            Ok(Some(element))
        }
    })
    .await
}

pub(super) async fn make_factory(
    stream_config: &StreamConfig,
) -> AnyResult<(NeoMediaFactory, MpscReceiver<ClientData>)> {
    let (client_tx, client_rx) = mpsc(100);
    let factory = {
        let stream_config = stream_config.clone();

        NeoMediaFactory::new_with_callback(move |element| {
            clear_bin(&element)?;
            let vid = match stream_config.vid_format {
                VidFormat::None => {
                    // This should not be reachable
                    log::debug!("Building unknown during normal make factory");
                    build_unknown(&element, "black")?;
                    AnyResult::Ok(None)
                }
                VidFormat::H264 => {
                    let app = build_h264(&element, &stream_config)?;
                    app.set_callbacks(
                        AppSrcCallbacks::builder()
                            .seek_data(move |_, _seek_pos| true)
                            .build(),
                    );
                    AnyResult::Ok(Some(app))
                }
                VidFormat::H265 => {
                    let app = build_h265(&element, &stream_config)?;

                    app.set_callbacks(
                        AppSrcCallbacks::builder()
                            .seek_data(move |_, _seek_pos| true)
                            .build(),
                    );
                    AnyResult::Ok(Some(app))
                }
            }?;
            let aud = if matches!(stream_config.vid_format, VidFormat::None) {
                None
            } else {
                match stream_config.aud_format {
                    AudFormat::None => AnyResult::Ok(None),
                    AudFormat::Aac => {
                        let app = build_aac(&element, &stream_config)?;
                        app.set_callbacks(
                            AppSrcCallbacks::builder()
                                .seek_data(move |_, _seek_pos| true)
                                .build(),
                        );
                        AnyResult::Ok(Some(app))
                    }
                    AudFormat::Adpcm(block_size) => {
                        let app = build_adpcm(&element, block_size, &stream_config)?;
                        app.set_callbacks(
                            AppSrcCallbacks::builder()
                                .seek_data(move |_, _seek_pos| true)
                                .build(),
                        );
                        AnyResult::Ok(Some(app))
                    }
                }?
            };

            client_tx.blocking_send(ClientData {
                vid: vid.map(|app| ClientSourceData { app }),
                aud: aud.map(|app| ClientSourceData { app }),
            })?;
            Ok(Some(element))
        })
        .await
    }?;

    Ok((factory, client_rx))
}

fn clear_bin(bin: &Element) -> Result<()> {
    let bin = bin
        .clone()
        .dynamic_cast::<Bin>()
        .map_err(|_| anyhow!("Media source's element should be a bin"))?;
    // Clear the autogenerated ones
    log::debug!("Clearing old elements");
    for element in bin.iterate_elements().into_iter().flatten() {
        bin.remove(&element)?;
    }

    Ok(())
}

fn build_unknown(bin: &Element, pattern: &str) -> Result<()> {
    let bin = bin
        .clone()
        .dynamic_cast::<Bin>()
        .map_err(|_| anyhow!("Media source's element should be a bin"))?;
    log::debug!("Building Unknown Pipeline");
    let source = make_element("videotestsrc", "testvidsrc")?;
    source.set_property_from_str("pattern", pattern);
    source.set_property("num-buffers", 500i32); // Send buffers then EOS
    let queue = make_queue("queue0", 1024 * 1024 * 4)?;

    let overlay = make_element("textoverlay", "overlay")?;
    overlay.set_property("text", "Stream not Ready");
    overlay.set_property_from_str("valignment", "top");
    overlay.set_property_from_str("halignment", "left");
    overlay.set_property("font-desc", "Sans, 16");
    let encoder = make_element("jpegenc", "encoder")?;
    let payload = make_element("rtpjpegpay", "pay0")?;

    bin.add_many([&source, &queue, &overlay, &encoder, &payload])?;
    source.link_filtered(
        &queue,
        &Caps::builder("video/x-raw")
            .field("format", "YUY2")
            .field("width", 896i32)
            .field("height", 512i32)
            .field("framerate", gstreamer::Fraction::new(25, 1))
            .build(),
    )?;
    Element::link_many([&queue, &overlay, &encoder, &payload])?;

    Ok(())
}

fn build_h264(bin: &Element, stream_config: &StreamConfig) -> Result<AppSrc> {
    let buffer_size = buffer_size(stream_config.bitrate);
    log::debug!("buffer_size: {buffer_size}");
    let bin = bin
        .clone()
        .dynamic_cast::<Bin>()
        .map_err(|_| anyhow!("Media source's element should be a bin"))?;
    log::debug!("Building H264 Pipeline");
    let source = make_element("appsrc", "vidsrc")?
        .dynamic_cast::<AppSrc>()
        .map_err(|_| anyhow!("Cannot cast to appsrc."))?;

    source.set_is_live(true);
    source.set_block(false);
    source.set_min_latency(0);
    source.set_property("emit-signals", false);
    source.set_max_bytes(buffer_size as u64 * 3);
    source.set_do_timestamp(true);
    source.set_stream_type(AppStreamType::Seekable);

    let source = source
        .dynamic_cast::<Element>()
        .map_err(|_| anyhow!("Cannot cast back"))?;
    let queue = make_queue("source_queue", buffer_size)?;
    let parser = make_element("h264parse", "parser")?;
    let stamper = make_element("h264timestamper", "stamper")?;
    let payload = make_element("rtph264pay", "pay0")?;
    bin.add_many([&source, &queue, &parser, &stamper, &payload])?;
    Element::link_many([&source, &queue, &parser, &stamper, &payload])?;

    let source = source
        .dynamic_cast::<AppSrc>()
        .map_err(|_| anyhow!("Cannot convert appsrc"))?;
    Ok(source)
}

fn build_h265(bin: &Element, stream_config: &StreamConfig) -> Result<AppSrc> {
    let buffer_size = buffer_size(stream_config.bitrate);
    log::debug!("buffer_size: {buffer_size}");
    let bin = bin
        .clone()
        .dynamic_cast::<Bin>()
        .map_err(|_| anyhow!("Media source's element should be a bin"))?;
    log::debug!("Building H265 Pipeline");
    let source = make_element("appsrc", "vidsrc")?
        .dynamic_cast::<AppSrc>()
        .map_err(|_| anyhow!("Cannot cast to appsrc."))?;
    source.set_is_live(true);
    source.set_block(false);
    source.set_min_latency(0);
    source.set_property("emit-signals", false);
    source.set_max_bytes(buffer_size as u64 * 3);
    source.set_do_timestamp(true);
    source.set_stream_type(AppStreamType::Seekable);

    let source = source
        .dynamic_cast::<Element>()
        .map_err(|_| anyhow!("Cannot cast back"))?;
    let queue = make_queue("source_queue", buffer_size)?;
    let parser = make_element("h265parse", "parser")?;
    let stamper = make_element("h265timestamper", "stamper")?;
    let payload = make_element("rtph265pay", "pay0")?;
    bin.add_many([&source, &queue, &parser, &stamper, &payload])?;
    Element::link_many([&source, &queue, &parser, &stamper, &payload])?;

    let source = source
        .dynamic_cast::<AppSrc>()
        .map_err(|_| anyhow!("Cannot convert appsrc"))?;
    Ok(source)
}

fn build_aac(bin: &Element, stream_config: &StreamConfig) -> Result<AppSrc> {
    let buffer_size = buffer_size(stream_config.bitrate);
    log::debug!("buffer_size: {buffer_size}");
    let bin = bin
        .clone()
        .dynamic_cast::<Bin>()
        .map_err(|_| anyhow!("Media source's element should be a bin"))?;
    log::debug!("Building Aac pipeline");
    let source = make_element("appsrc", "audsrc")?
        .dynamic_cast::<AppSrc>()
        .map_err(|_| anyhow!("Cannot cast to appsrc."))?;

    source.set_is_live(true);
    source.set_block(false);
    source.set_min_latency(0);
    source.set_property("emit-signals", false);
    source.set_max_bytes(buffer_size as u64 * 3);
    source.set_do_timestamp(true);
    source.set_stream_type(AppStreamType::Seekable);

    let source = source
        .dynamic_cast::<Element>()
        .map_err(|_| anyhow!("Cannot cast back"))?;

    let queue = make_queue("audqueue", buffer_size)?;
    let parser = make_element("aacparse", "audparser")?;
    let decoder = match make_element("faad", "auddecoder_faad") {
        Ok(ele) => Ok(ele),
        Err(_) => make_element("avdec_aac", "auddecoder_avdec_aac"),
    }?;

    // The fallback
    let silence = make_element("audiotestsrc", "audsilence")?;
    silence.set_property_from_str("wave", "silence");
    let fallback_switch = make_element("fallbackswitch", "audfallbackswitch");
    if let Ok(fallback_switch) = fallback_switch.as_ref() {
        fallback_switch.set_property("timeout", 3u64 * 1_000_000_000u64);
        fallback_switch.set_property("immediate-fallback", true);
    }

    let encoder = make_element("audioconvert", "audencoder")?;
    let payload = make_element("rtpL16pay", "pay1")?;

    bin.add_many([&source, &queue, &parser, &decoder, &encoder, &payload])?;
    if let Ok(fallback_switch) = fallback_switch.as_ref() {
        bin.add_many([&silence, fallback_switch])?;
        Element::link_many([
            &source,
            &queue,
            &parser,
            &decoder,
            fallback_switch,
            &encoder,
            &payload,
        ])?;
        Element::link_many([&silence, fallback_switch])?;
    } else {
        Element::link_many([&source, &queue, &parser, &decoder, &encoder, &payload])?;
    }

    let source = source
        .dynamic_cast::<AppSrc>()
        .map_err(|_| anyhow!("Cannot convert appsrc"))?;
    Ok(source)
}

fn build_adpcm(bin: &Element, block_size: u32, stream_config: &StreamConfig) -> Result<AppSrc> {
    let buffer_size = buffer_size(stream_config.bitrate);
    log::debug!("buffer_size: {buffer_size}");
    let bin = bin
        .clone()
        .dynamic_cast::<Bin>()
        .map_err(|_| anyhow!("Media source's element should be a bin"))?;
    log::debug!("Building Adpcm pipeline");
    // Original command line
    // caps=audio/x-adpcm,layout=dvi,block_align={},channels=1,rate=8000
    // ! queue silent=true max-size-bytes=10485760 min-threshold-bytes=1024
    // ! adpcmdec
    // ! audioconvert
    // ! rtpL16pay name=pay1

    let source = make_element("appsrc", "audsrc")?
        .dynamic_cast::<AppSrc>()
        .map_err(|_| anyhow!("Cannot cast to appsrc."))?;
    source.set_is_live(true);
    source.set_block(false);
    source.set_min_latency(0);
    source.set_property("emit-signals", false);
    source.set_max_bytes(buffer_size as u64 * 3);
    source.set_do_timestamp(true);
    source.set_stream_type(AppStreamType::Seekable);

    source.set_caps(Some(
        &Caps::builder("audio/x-adpcm")
            .field("layout", "div")
            .field("block_align", block_size as i32)
            .field("channels", 1i32)
            .field("rate", 8000i32)
            .build(),
    ));

    let source = source
        .dynamic_cast::<Element>()
        .map_err(|_| anyhow!("Cannot cast back"))?;

    let queue = make_queue("audqueue", buffer_size)?;
    let decoder = make_element("decodebin", "auddecoder")?;
    let encoder = make_element("audioconvert", "audencoder")?;
    let payload = make_element("rtpL16pay", "pay1")?;

    bin.add_many([&source, &queue, &decoder, &encoder, &payload])?;
    Element::link_many([&source, &queue, &decoder])?;
    Element::link_many([&encoder, &payload])?;
    decoder.connect_pad_added(move |_element, pad| {
        let sink_pad = encoder
            .static_pad("sink")
            .expect("Encoder is missing its pad");
        pad.link(&sink_pad)
            .expect("Failed to link ADPCM decoder to encoder");
    });

    let source = source
        .dynamic_cast::<AppSrc>()
        .map_err(|_| anyhow!("Cannot convert appsrc"))?;
    Ok(source)
}

// Convenice funcion to make an element or provide a message
// about what plugin is missing
fn make_element(kind: &str, name: &str) -> AnyResult<Element> {
    ElementFactory::make_with_name(kind, Some(name)).with_context(|| {
        let plugin = match kind {
            "appsrc" => "app (gst-plugins-base)",
            "audioconvert" => "audioconvert (gst-plugins-base)",
            "adpcmdec" => "Required for audio",
            "h264parse" => "videoparsersbad (gst-plugins-bad)",
            "h265parse" => "videoparsersbad (gst-plugins-bad)",
            "rtph264pay" => "rtp (gst-plugins-good)",
            "rtph265pay" => "rtp (gst-plugins-good)",
            "rtpjitterbuffer" => "rtp (gst-plugins-good)",
            "aacparse" => "audioparsers (gst-plugins-good)",
            "rtpL16pay" => "rtp (gst-plugins-good)",
            "x264enc" => "x264 (gst-plugins-ugly)",
            "x265enc" => "x265 (gst-plugins-bad)",
            "avdec_h264" => "libav (gst-libav)",
            "avdec_h265" => "libav (gst-libav)",
            "videotestsrc" => "videotestsrc (gst-plugins-base)",
            "imagefreeze" => "imagefreeze (gst-plugins-good)",
            "audiotestsrc" => "audiotestsrc (gst-plugins-base)",
            "decodebin" => "playback (gst-plugins-good)",
            _ => "Unknown",
        };
        format!(
            "Missing required gstreamer plugin `{}` for `{}` element",
            plugin, kind
        )
    })
}

#[allow(dead_code)]
fn make_dbl_queue(name: &str, buffer_size: u32) -> AnyResult<Element> {
    let queue = make_element("queue", &format!("queue1_{}", name))?;
    queue.set_property("max-size-bytes", buffer_size);
    queue.set_property("max-size-buffers", 0u32);
    queue.set_property("max-size-time", 0u64);
    // queue.set_property(
    //     "max-size-time",
    //     std::convert::TryInto::<u64>::try_into(tokio::time::Duration::from_secs(5).as_nanos())
    //         .unwrap_or(0),
    // );

    let queue2 = make_element("queue2", &format!("queue2_{}", name))?;
    queue2.set_property("max-size-bytes", buffer_size * 2u32 / 3u32);
    queue.set_property("max-size-buffers", 0u32);
    queue.set_property("max-size-time", 0u64);
    queue2.set_property(
        "max-size-time",
        std::convert::TryInto::<u64>::try_into(tokio::time::Duration::from_secs(5).as_nanos())
            .unwrap_or(0),
    );
    queue2.set_property("use-buffering", false);

    let bin = gstreamer::Bin::builder().name(name).build();
    bin.add_many([&queue, &queue2])?;
    Element::link_many([&queue, &queue2])?;

    let pad = queue
        .static_pad("sink")
        .expect("Failed to get a static pad from queue.");
    let ghost_pad = GhostPad::builder_with_target(&pad).unwrap().build();
    ghost_pad.set_active(true)?;
    bin.add_pad(&ghost_pad)?;

    let pad = queue2
        .static_pad("src")
        .expect("Failed to get a static pad from queue2.");
    let ghost_pad = GhostPad::builder_with_target(&pad).unwrap().build();
    ghost_pad.set_active(true)?;
    bin.add_pad(&ghost_pad)?;

    let bin = bin
        .dynamic_cast::<Element>()
        .map_err(|_| anyhow!("Cannot convert bin"))?;
    Ok(bin)
}

fn make_queue(name: &str, buffer_size: u32) -> AnyResult<Element> {
    let queue = make_element("queue", &format!("queue1_{}", name))?;
    queue.set_property("max-size-bytes", buffer_size);
    queue.set_property("max-size-buffers", 0u32);
    queue.set_property("max-size-time", 0u64);
    queue.set_property(
        "max-size-time",
        std::convert::TryInto::<u64>::try_into(tokio::time::Duration::from_secs(5).as_nanos())
            .unwrap_or(0),
    );
    Ok(queue)
}

fn buffer_size(bitrate: u32) -> u32 {
    std::cmp::max(bitrate * 15u32 / 8u32, 4u32 * 1024u32 * 1024u32)
}
