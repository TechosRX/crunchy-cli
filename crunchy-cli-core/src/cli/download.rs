use crate::cli::log::tab_info;
use crate::cli::utils::{
    download_segments, find_multiple_seasons_with_same_number, find_resolution,
    interactive_season_choosing, FFmpegPreset,
};
use crate::utils::context::Context;
use crate::utils::format::Format;
use crate::utils::log::progress;
use crate::utils::os::{free_file, has_ffmpeg, is_special_file, tempfile};
use crate::utils::parse::{parse_url, UrlFilter};
use crate::utils::sort::{sort_formats_after_seasons, sort_seasons_after_number};
use crate::utils::subtitle::download_subtitle;
use crate::utils::video::get_video_length;
use crate::Execute;
use anyhow::{bail, Result};
use crunchyroll_rs::media::{Resolution, StreamSubtitle, VariantData};
use crunchyroll_rs::{
    Episode, Locale, Media, MediaCollection, Movie, MovieListing, Season, Series,
};
use log::{debug, error, info, warn};
use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Debug, clap::Parser)]
#[clap(about = "Download a video")]
#[command(arg_required_else_help(true))]
pub struct Download {
    #[arg(help = format!("Audio language. Can only be used if the provided url(s) point to a series. \
    Available languages are: {}", Locale::all().into_iter().map(|l| l.to_string()).collect::<Vec<String>>().join(", ")))]
    #[arg(long_help = format!("Audio language. Can only be used if the provided url(s) point to a series. \
    Available languages are:\n{}", Locale::all().into_iter().map(|l| format!("{:<6} → {}", l.to_string(), l.to_human_readable())).collect::<Vec<String>>().join("\n  ")))]
    #[arg(short, long, default_value_t = crate::utils::locale::system_locale())]
    audio: Locale,
    #[arg(help = format!("Subtitle language. Available languages are: {}", Locale::all().into_iter().map(|l| l.to_string()).collect::<Vec<String>>().join(", ")))]
    #[arg(long_help = format!("Subtitle language. If set, the subtitle will be burned into the video and cannot be disabled. \
    Available languages are: {}", Locale::all().into_iter().map(|l| l.to_string()).collect::<Vec<String>>().join(", ")))]
    #[arg(short, long)]
    subtitle: Option<Locale>,

    #[arg(help = "Name of the output file")]
    #[arg(long_help = "Name of the output file.\
    If you use one of the following pattern they will get replaced:\n  \
      {title}                   → Title of the video\n  \
      {series_name}             → Name of the series\n  \
      {season_name}             → Name of the season\n  \
      {audio}                   → Audio language of the video\n  \
      {resolution}              → Resolution of the video\n  \
      {season_number}           → Number of the season\n  \
      {episode_number}          → Number of the episode\n  \
      {relative_episode_number} → Number of the episode relative to its season\
      {series_id}               → ID of the series\n  \
      {season_id}               → ID of the season\n  \
      {episode_id}              → ID of the episode")]
    #[arg(short, long, default_value = "{title}.mp4")]
    output: String,

    #[arg(help = "Video resolution")]
    #[arg(long_help = "The video resolution.\
    Can either be specified via the pixels (e.g. 1920x1080), the abbreviation for pixels (e.g. 1080p) or 'common-use' words (e.g. best). \
    Specifying the exact pixels is not recommended, use one of the other options instead. \
    Crunchyroll let you choose the quality with pixel abbreviation on their clients, so you might be already familiar with the available options. \
    The available common-use words are 'best' (choose the best resolution available) and 'worst' (worst resolution available)")]
    #[arg(short, long, default_value = "best")]
    #[arg(value_parser = crate::utils::clap::clap_parse_resolution)]
    resolution: Resolution,

    #[arg(help = format!("Presets for video converting. Can be used multiple times. \
    Available presets: \n  {}", FFmpegPreset::available_matches_human_readable().join("\n  ")))]
    #[arg(long_help = format!("Presets for video converting. Can be used multiple times. \
    Generally used to minify the file size with keeping (nearly) the same quality. \
    It is recommended to only use this if you download videos with high resolutions since low resolution videos tend to result in a larger file with any of the provided presets. \
    Available presets: \n  {}", FFmpegPreset::available_matches_human_readable().join("\n  ")))]
    #[arg(long)]
    #[arg(value_parser = FFmpegPreset::parse)]
    ffmpeg_preset: Option<FFmpegPreset>,

    #[arg(help = "Skip files which are already existing")]
    #[arg(long, default_value_t = false)]
    skip_existing: bool,

    #[arg(help = "Ignore interactive input")]
    #[arg(short, long, default_value_t = false)]
    yes: bool,

    #[arg(help = "Url(s) to Crunchyroll episodes or series")]
    urls: Vec<String>,
}

#[async_trait::async_trait(?Send)]
impl Execute for Download {
    fn pre_check(&mut self) -> Result<()> {
        if !has_ffmpeg() {
            bail!("FFmpeg is needed to run this command")
        } else if Path::new(&self.output)
            .extension()
            .unwrap_or_default()
            .is_empty()
            && self.output != "-"
        {
            bail!("No file extension found. Please specify a file extension (via `-o`) for the output file")
        }

        if self.subtitle.is_some() {
            if let Some(ext) = Path::new(&self.output).extension() {
                if ext.to_string_lossy() != "mp4" {
                    warn!("Detected a non mp4 output container. Adding subtitles may take a while")
                }
            }
        }

        Ok(())
    }

    async fn execute(self, ctx: Context) -> Result<()> {
        let mut parsed_urls = vec![];

        for (i, url) in self.urls.iter().enumerate() {
            let progress_handler = progress!("Parsing url {}", i + 1);
            match parse_url(&ctx.crunchy, url.clone(), true).await {
                Ok((media_collection, url_filter)) => {
                    parsed_urls.push((media_collection, url_filter));
                    progress_handler.stop(format!("Parsed url {}", i + 1))
                }
                Err(e) => bail!("url {} could not be parsed: {}", url, e),
            }
        }

        for (i, (media_collection, url_filter)) in parsed_urls.into_iter().enumerate() {
            let progress_handler = progress!("Fetching series details");
            let formats = match media_collection {
                MediaCollection::Series(series) => {
                    debug!("Url {} is series ({})", i + 1, series.title);
                    formats_from_series(&self, series, &url_filter).await?
                }
                MediaCollection::Season(season) => {
                    debug!(
                        "Url {} is season {} ({})",
                        i + 1,
                        season.metadata.season_number,
                        season.title
                    );
                    formats_from_season(&self, season, &url_filter).await?
                }
                MediaCollection::Episode(episode) => {
                    debug!(
                        "Url {} is episode {} ({}) of season {} ({}) of {}",
                        i + 1,
                        episode.metadata.episode_number,
                        episode.title,
                        episode.metadata.season_number,
                        episode.metadata.season_title,
                        episode.metadata.series_title
                    );
                    format_from_episode(&self, &episode, &url_filter, None, false)
                        .await?
                        .map(|fmt| vec![fmt])
                }
                MediaCollection::MovieListing(movie_listing) => {
                    debug!("Url {} is movie listing ({})", i + 1, movie_listing.title);
                    format_from_movie_listing(&self, movie_listing, &url_filter).await?
                }
                MediaCollection::Movie(movie) => {
                    debug!("Url {} is movie ({})", i + 1, movie.title);
                    format_from_movie(&self, movie, &url_filter)
                        .await?
                        .map(|fmt| vec![fmt])
                }
            };

            let Some(formats) = formats else {
                progress_handler.stop(format!("Skipping url {} (no matching episodes found)", i + 1));
                continue;
            };
            progress_handler.stop(format!("Loaded series information for url {}", i + 1));

            if log::max_level() == log::Level::Debug {
                let seasons = sort_formats_after_seasons(formats.clone());
                debug!("Series has {} seasons", seasons.len());
                for (i, season) in seasons.into_iter().enumerate() {
                    info!("Season {} ({})", i + 1, season.get(0).unwrap().season_title);
                    for format in season {
                        info!(
                            "{}: {}px, {:.02} FPS (S{:02}E{:02})",
                            format.title,
                            format.stream.resolution,
                            format.stream.fps,
                            format.season_number,
                            format.episode_number,
                        )
                    }
                }
            } else {
                for season in sort_formats_after_seasons(formats.clone()) {
                    let first = season.get(0).unwrap();
                    info!(
                        "{} Season {} ({})",
                        first.series_name, first.season_number, first.season_title
                    );

                    for (i, format) in season.into_iter().enumerate() {
                        tab_info!(
                            "{}. {} » {}px, {:.2} FPS (S{:02}E{:02})",
                            i + 1,
                            format.title,
                            format.stream.resolution,
                            format.stream.fps,
                            format.season_number,
                            format.episode_number
                        )
                    }
                }
            }

            for format in formats {
                let formatted_path = format.format_path((&self.output).into(), true);
                let (path, changed) = free_file(formatted_path.clone());

                if changed && self.skip_existing {
                    debug!(
                        "Skipping already existing file '{}'",
                        formatted_path.to_string_lossy()
                    );
                    continue;
                }

                info!(
                    "Downloading {} to '{}'",
                    format.title,
                    if is_special_file(&path) {
                        path.to_str().unwrap()
                    } else {
                        path.file_name().unwrap().to_str().unwrap()
                    }
                );
                tab_info!(
                    "Episode: S{:02}E{:02}",
                    format.season_number,
                    format.episode_number
                );
                tab_info!("Audio: {}", format.audio);
                tab_info!(
                    "Subtitles: {}",
                    self.subtitle
                        .clone()
                        .map_or("None".to_string(), |l| l.to_string())
                );
                tab_info!("Resolution: {}", format.stream.resolution);
                tab_info!("FPS: {:.2}", format.stream.fps);

                download_ffmpeg(
                    &ctx,
                    &self,
                    format.stream,
                    format.subtitles.get(0).cloned(),
                    path.to_path_buf(),
                )
                .await?;
            }
        }

        Ok(())
    }
}

async fn download_ffmpeg(
    ctx: &Context,
    download: &Download,
    variant_data: VariantData,
    subtitle: Option<StreamSubtitle>,
    mut target: PathBuf,
) -> Result<()> {
    let (input_presets, mut output_presets) = if let Some(preset) = download.ffmpeg_preset.clone() {
        preset.to_input_output_args()
    } else {
        (
            vec![],
            vec![
                "-c:v".to_string(),
                "copy".to_string(),
                "-c:a".to_string(),
                "copy".to_string(),
            ],
        )
    };

    // create parent directory if it does not exist
    if let Some(parent) = target.parent() {
        if !parent.exists() {
            std::fs::create_dir_all(parent)?
        }
    }

    let mut video_file = tempfile(".ts")?;
    download_segments(ctx, &mut video_file, None, variant_data).await?;
    let subtitle_file = if let Some(ref sub) = subtitle {
        let video_len = get_video_length(video_file.path().to_path_buf())?;
        Some(download_subtitle(sub.clone(), video_len).await?)
    } else {
        None
    };

    let stdout_tempfile = if target.to_string_lossy() == "-" {
        let file = tempfile(".mp4")?;
        target = file.path().to_path_buf();

        Some(file)
    } else {
        None
    };

    let subtitle_presets = if let Some(sub_file) = &subtitle_file {
        if target.extension().unwrap_or_default().to_string_lossy() == "mp4" {
            vec![
                "-i".to_string(),
                sub_file.to_string_lossy().to_string(),
                "-movflags".to_string(),
                "faststart".to_string(),
                "-c:s".to_string(),
                "mov_text".to_string(),
                "-disposition:s:s:0".to_string(),
                "forced".to_string(),
            ]
        } else {
            // remove '-c:v copy' and '-c:a copy' from output presets as its causes issues with
            // burning subs into the video
            let mut last = String::new();
            let mut remove_count = 0;
            for (i, s) in output_presets.clone().iter().enumerate() {
                if (last == "-c:v" || last == "-c:a") && s == "copy" {
                    // remove last
                    output_presets.remove(i - remove_count - 1);
                    remove_count += 1;
                    output_presets.remove(i - remove_count);
                    remove_count += 1;
                }
                last = s.clone();
            }

            vec![
                "-vf".to_string(),
                format!("subtitles={}", sub_file.to_string_lossy()),
            ]
        }
    } else {
        vec![]
    };

    let mut ffmpeg = Command::new("ffmpeg")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .arg("-y")
        .args(input_presets)
        .args(["-i", video_file.path().to_string_lossy().as_ref()])
        .args(subtitle_presets)
        .args(output_presets)
        .arg(target.to_str().unwrap())
        .spawn()?;

    let progress_handler = progress!("Generating output file");
    if !ffmpeg.wait()?.success() {
        bail!("{}", std::io::read_to_string(ffmpeg.stderr.unwrap())?)
    }
    progress_handler.stop("Output file generated");

    if let Some(mut stdout_file) = stdout_tempfile {
        let mut stdout = std::io::stdout();

        std::io::copy(&mut stdout_file, &mut stdout)?;
    }

    Ok(())
}

async fn formats_from_series(
    download: &Download,
    series: Media<Series>,
    url_filter: &UrlFilter,
) -> Result<Option<Vec<Format>>> {
    if !series.metadata.audio_locales.is_empty()
        && !series.metadata.audio_locales.contains(&download.audio)
    {
        error!(
            "Series {} is not available with {} audio",
            series.title, download.audio
        );
        return Ok(None);
    }

    let mut seasons = series.seasons().await?;

    // filter any season out which does not contain the specified audio language
    for season in sort_seasons_after_number(seasons.clone()) {
        // check if the current iterated season has the specified audio language
        if !season
            .iter()
            .any(|s| s.metadata.audio_locales.contains(&download.audio))
        {
            error!(
                "Season {} of series {} is not available with {} audio",
                season.first().unwrap().metadata.season_number,
                series.title,
                download.audio
            );
        }

        // remove all seasons with the wrong audio for the current iterated season number
        seasons.retain(|s| {
            s.metadata.season_number != season.first().unwrap().metadata.season_number
                || s.metadata.audio_locales.contains(&download.audio)
        });
        // remove seasons which match the url filter. this is mostly done to not trigger the
        // interactive season choosing when dupilcated seasons are excluded by the filter
        seasons.retain(|s| url_filter.is_season_valid(s.metadata.season_number))
    }

    if !download.yes && !find_multiple_seasons_with_same_number(&seasons).is_empty() {
        info!(target: "progress_end", "Fetched seasons");
        seasons = interactive_season_choosing(seasons);
        info!(target: "progress", "Fetching series details")
    }

    let mut formats = vec![];
    for season in seasons {
        if let Some(fmts) = formats_from_season(download, season, url_filter).await? {
            formats.extend(fmts)
        }
    }

    Ok(some_vec_or_none(formats))
}

async fn formats_from_season(
    download: &Download,
    season: Media<Season>,
    url_filter: &UrlFilter,
) -> Result<Option<Vec<Format>>> {
    if !url_filter.is_season_valid(season.metadata.season_number) {
        return Ok(None);
    } else if !season.metadata.audio_locales.contains(&download.audio) {
        error!(
            "Season {} ({}) is not available with {} audio",
            season.metadata.season_number, season.title, download.audio
        );
        return Ok(None);
    }

    let mut formats = vec![];

    let episodes = season.episodes().await?;
    for episode in episodes.iter() {
        if let Some(fmt) =
            format_from_episode(download, &episode, url_filter, Some(&episodes), true).await?
        {
            formats.push(fmt)
        }
    }

    Ok(some_vec_or_none(formats))
}

async fn format_from_episode(
    download: &Download,
    episode: &Media<Episode>,
    url_filter: &UrlFilter,
    season_episodes: Option<&Vec<Media<Episode>>>,
    filter_audio: bool,
) -> Result<Option<Format>> {
    if filter_audio && episode.metadata.audio_locale != download.audio {
        error!(
            "Episode {} ({}) of season {} ({}) of {} has no {} audio",
            episode.metadata.episode_number,
            episode.title,
            episode.metadata.season_number,
            episode.metadata.season_title,
            episode.metadata.series_title,
            download.audio
        );
        return Ok(None);
    } else if !url_filter.is_episode_valid(
        episode.metadata.episode_number,
        episode.metadata.season_number,
    ) {
        return Ok(None);
    }

    let streams = episode.streams().await?;
    let streaming_data = streams.hls_streaming_data(None).await?;
    let subtitle = if let Some(subtitle) = &download.subtitle {
        if let Some(sub) = streams.subtitles.get(subtitle) {
            Some(sub.clone())
        } else {
            error!(
                "Episode {} ({}) of season {} ({}) of {} has no {} subtitles",
                episode.metadata.episode_number,
                episode.title,
                episode.metadata.season_number,
                episode.metadata.season_title,
                episode.metadata.series_title,
                subtitle
            );
            return Ok(None);
        }
    } else {
        None
    };

    let Some(stream) = find_resolution(streaming_data, &download.resolution) else {
        bail!(
            "Resolution ({}x{}) is not available for episode {} ({}) of season {} ({}) of {}",
            download.resolution.width,
            download.resolution.height,
            episode.metadata.episode_number,
            episode.title,
            episode.metadata.season_number,
            episode.metadata.season_title,
            episode.metadata.series_title
        )
    };

    let season_eps = if Format::has_relative_episodes_fmt(&download.output) {
        if let Some(eps) = season_episodes {
            Cow::from(eps)
        } else {
            Cow::from(episode.season().await?.episodes().await?)
        }
    } else {
        Cow::from(vec![])
    };

    Ok(Some(Format::new_from_episode(
        episode,
        &season_eps.to_vec(),
        stream,
        subtitle.map_or_else(|| vec![], |s| vec![s]),
    )))
}

async fn format_from_movie_listing(
    download: &Download,
    movie_listing: Media<MovieListing>,
    url_filter: &UrlFilter,
) -> Result<Option<Vec<Format>>> {
    let mut formats = vec![];

    for movie in movie_listing.movies().await? {
        if let Some(fmt) = format_from_movie(download, movie, url_filter).await? {
            formats.push(fmt)
        }
    }

    Ok(some_vec_or_none(formats))
}

async fn format_from_movie(
    download: &Download,
    movie: Media<Movie>,
    _: &UrlFilter,
) -> Result<Option<Format>> {
    let streams = movie.streams().await?;
    let mut streaming_data = if let Some(subtitle) = &download.subtitle {
        if !streams.subtitles.keys().cloned().any(|x| &x == subtitle) {
            error!("Movie {} has no {} subtitles", movie.title, subtitle);
            return Ok(None);
        }
        streams.hls_streaming_data(Some(subtitle.clone())).await?
    } else {
        streams.hls_streaming_data(None).await?
    };

    streaming_data.sort_by(|a, b| a.resolution.width.cmp(&b.resolution.width).reverse());
    let stream = {
        match download.resolution.height {
            u64::MAX => streaming_data.into_iter().next().unwrap(),
            u64::MIN => streaming_data.into_iter().last().unwrap(),
            _ => {
                if let Some(streaming_data) = streaming_data.into_iter().find(|v| {
                    download.resolution.height == u64::MAX
                        || v.resolution.height == download.resolution.height
                }) {
                    streaming_data
                } else {
                    bail!(
                        "Resolution ({}x{}) is not available for movie {}",
                        download.resolution.width,
                        download.resolution.height,
                        movie.title
                    )
                }
            }
        }
    };

    Ok(Some(Format::new_from_movie(&movie, stream)))
}

fn some_vec_or_none<T>(v: Vec<T>) -> Option<Vec<T>> {
    if v.is_empty() {
        None
    } else {
        Some(v)
    }
}
