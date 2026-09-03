use anyhow::{Context, Result};
use ffmpeg_next as ffmpeg;

fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    ffmpeg::init()
        .context("ffmpeg init failed")?;

    let url = std::env::args()
        .nth(1)
        .context("usage: ferrumview <hls-url>")?;

    let mut input =
        ffmpeg::format::input(&url)
            .context("failed to open HLS stream")?;

    let video = input
        .streams()
        .best(ffmpeg::media::Type::Video)
        .context("no video stream")?;

    let video_index = video.index();

    let params = video.parameters();

    println!(
        "video stream index={} codec={:?}",
        video_index,
        params.id()
    );

    let mut packet_count = 0u64;
    let mut byte_count = 0u64;

    for (stream, packet) in input.packets() {
        if stream.index() != video_index {
            continue;
        }

        let Some(data) = packet.data() else {
            continue;
        };

        packet_count += 1;
        byte_count += data.len() as u64;

        if packet_count % 25 == 0 {
            println!(
                "packets={} total={} KB pts={:?} dts={:?} key={}",
                packet_count,
                byte_count / 1024,
                packet.pts(),
                packet.dts(),
                packet.is_key()
            );
        }
    }

    Ok(())
}