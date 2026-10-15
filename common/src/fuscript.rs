use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering::SeqCst;
use parking_lot::Mutex;

// Resolve's scripting server is not safe against concurrent fuscript connections
static FUSCRIPT_LOCK: Mutex<()> = Mutex::new(());

const FAILED_MSG: &str = "This feature relies on external scripting and is only available in paid Resolve Studio. You have to allow executing scripts:\n
Set \"Preferences -> General -> External scripting using\" to \"Local\".\n\n
It must be the currently displayed video on the timeline.\n
It is also impossible to query file path on a compound clip.\n\nIn any case, you can just select the video or project file using the \"Browse\" button.";

fn replace_frame_count(input: &str) -> String {
    use regex::Regex;
    let re = Regex::new(r"\[(\d+)-(\d+)\]").unwrap();

    re.replace_all(input, |caps: &regex::Captures| {
        format!("{}", &caps[1])
    }).to_string()
}

#[allow(dead_code)]
#[derive(Clone, Debug)]
pub struct CurrentFileInfo {
    pub file_path: String,
    pub project_path: Option<String>,
    pub fps: f64,
    pub duration_s: f64,
    pub frame_count: usize,
    pub width: usize,
    pub height: usize,
    pub pixel_aspect_ratio: String,
    /// Trim of the timeline item: (left offset into the source media, duration), in frames
    pub trim_frames: Option<(f64, f64)>,
    pub scaling: ResolveScaling,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResolveScaling {
    Fit,
    Other,
}

impl ResolveScaling {
    fn from_values(clip_scaling: Option<i32>, inherited: Option<&str>) -> Self {
        match clip_scaling {
            Some(2) => Self::Fit,
            Some(0) => match inherited.unwrap_or_default().to_ascii_lowercase().as_str() {
                "scaletofit" => Self::Fit,
                _ => Self::Other,
            },
            _ => Self::Other,
        }
    }
}
impl CurrentFileInfo {
    pub fn get_fuscript() -> Option<std::path::PathBuf> {
        if cfg!(target_os = "windows") {
            Some(std::path::Path::new("fuscript.exe").to_path_buf())
        } else if cfg!(target_os = "macos") {
            Some(std::path::Path::new("../Libraries/Fusion/fuscript").to_path_buf())
        } else if cfg!(target_os = "linux") {
            let p1 = std::path::Path::new("../libs/Fusion/fuscript");
            let p2 = std::path::Path::new("./libs/Fusion/fuscript");
            if p1.exists() { return Some(p1.to_path_buf()); }
            if p2.exists() { return Some(p2.to_path_buf()); }
            None
        } else {
            None
        }
    }
    pub fn is_available() -> bool {
        Self::get_fuscript().map(|x| x.exists()).unwrap_or_default()
    }
    /// Queries the current timeline item via Resolve's scripting API.
    /// `silent`: automatic (non user-initiated) query — no error dialog and no forced re-render.
    ///
    /// Retries a few times: right after a timeline edit (trimming, re-adding the effect),
    /// `GetCurrentVideoItem()` briefly returns nil while Resolve is busy.
    pub fn query(current_file_info: Arc<Mutex<Option<Self>>>, current_file_info_pending: Arc<AtomicBool>, silent: bool) {
        std::thread::spawn(move || {
            fn new_cmd() -> std::process::Command {
                let cmd = std::process::Command::new(CurrentFileInfo::get_fuscript().unwrap());
                #[cfg(target_os = "windows")]
                let cmd = { let mut cmd = cmd; use std::os::windows::process::CommandExt; cmd.creation_flags(0x08000000); cmd }; // CREATE_NO_WINDOW
                cmd
            }

            let script = "project = Resolve():GetProjectManager():GetCurrentProject(); timeline = project:GetCurrentTimeline(); i = timeline:GetCurrentVideoItem();
                              p = i:GetMediaPoolItem():GetClipProperty();
                              print(p['FPS']);print(p['Frames']);print(p['Duration']);print(p['PAR']);print(p['Resolution']);print(p['File Path']);
                              ok_s, ss = pcall(function() return i:GetSourceStartFrame() end); ok_e, se = pcall(function() return i:GetSourceEndFrame() end);
                              if ok_s and ok_e and ss ~= nil and se ~= nil and se > ss then print(ss); print(se - ss);
                              else r = 1; tf = tonumber(timeline:GetSetting('timelineFrameRate')); cf = tonumber(p['FPS']); if tf and cf and tf > 0 then r = cf / tf end;
                              print(i:GetLeftOffset() * r); print(i:GetDuration() * r) end;
                              print(i:GetProperty('Scaling'));
                              inherited = timeline:GetSetting('timelineInputResMismatchBehavior');
                              if inherited == nil or inherited == '' then inherited = project:GetSetting('timelineInputResMismatchBehavior') end;
                              print(inherited or '');";

            let mut stdout = String::new();
            let mut stderr = String::new();
            let mut lines_owned: Vec<String> = Vec::new();
            for attempt in 0..8 {
                if attempt > 0 { std::thread::sleep(std::time::Duration::from_millis(700)); }
                let output = { let _guard = FUSCRIPT_LOCK.lock(); new_cmd().args(["-q", "-l", "lua", "-x", &script]).output() };
                let Ok(out) = output else { return; };
                stdout = String::from_utf8(out.stdout).unwrap_or_default();
                stderr = String::from_utf8(out.stderr).unwrap_or_default();
                // There is a weird bug in DaVinci Resolve fuscript that it complains about
                // missing python2 even regardless of explicitly specified `-l lua` argument.
                // The error message itself is a subject to localization, so it can't be hardcoded in whole.
                // See https://github.com/gyroflow/gyroflow-plugins/issues/24
                fn is_missing_python2(line: &str) -> bool {
                    line.starts_with("sh:") && line.contains("python2:")
                }
                let ok = stderr.trim().lines().filter(|line| !is_missing_python2(line)).count() == 0
                    && stdout.trim().lines().count() >= 6;
                if ok {
                    lines_owned = stdout.trim().lines().map(|x| x.to_string()).collect();
                    break;
                }
                log::debug!("fuscript attempt {attempt} failed, stderr: {}", stderr.trim());
            }
            let lines: Vec<&str> = lines_owned.iter().map(|x| x.as_str()).collect();
            {
                if lines.len() >= 6 {
                    let fps = lines[0].parse::<f64>().unwrap_or_default();
                    let frame_count = lines[1].parse::<usize>().unwrap_or_default();
                    let duration_s = Self::parse_duration(lines[2], fps);
                    let par = lines[3];
                    let resolution = lines[4].split("x").filter_map(|x| x.parse::<usize>().ok()).collect::<Vec<_>>();
                    let file_path = replace_frame_count(lines[5]);
                    // Trim in SOURCE frames (start, length). Optional: older Resolve versions may not support these APIs
                    let trim_frames = match (lines.get(6).and_then(|x| x.parse::<f64>().ok()), lines.get(7).and_then(|x| x.parse::<f64>().ok())) {
                        (Some(left), Some(duration)) if duration > 0.0 => Some((left, duration)),
                        _ => None
                    };
                    let scaling = ResolveScaling::from_values(
                        lines.get(8).and_then(|x| x.parse::<i32>().ok()),
                        lines.get(9).copied(),
                    );
                    if fps > 0.0 && frame_count > 0 && duration_s > 0.0 && !file_path.is_empty() {
                        let info = Self {
                            file_path: file_path.to_string(),
                            fps,
                            duration_s,
                            frame_count,
                            width: *resolution.get(0).unwrap_or(&0),
                            height: *resolution.get(1).unwrap_or(&0),
                            pixel_aspect_ratio: par.to_string(),
                            project_path: crate::GyroflowPluginBase::get_project_path(&file_path),
                            trim_frames,
                            scaling,
                        };
                        log::debug!("{info:#?}");
                        *current_file_info.lock() = Some(info);
                        current_file_info_pending.store(true, SeqCst);

                        if !silent {
                            trigger_render();
                        }
                    }
                } else {
                    log::debug!("fuscript stdout: {stdout}");
                    log::debug!("fuscript stderr: {stderr}");
                    if !silent {
                        rfd::MessageDialog::new()
                            .set_title("Failed to query current video file path.")
                            .set_description(FAILED_MSG)
                            .set_level(rfd::MessageLevel::Warning)
                            .show();
                    }
                }
            }
        });
    }

    fn parse_duration(v: &str, fps: f64) -> f64 {
        let parts = v.replace(";", ":").split(':').filter_map(|x| x.parse::<f64>().ok()).collect::<Vec<_>>();
        if parts.len() == 4 {
            parts[0] * 60.0 * 60.0 + // h
            parts[1] * 60.0 + // m
            parts[2] + // s
            parts[3] / fps.max(1.0)
        } else {
            0.0
        }
    }
}

/// Makes Resolve render the current timeline item again, by setting one of its properties to its own value
pub fn trigger_render() {
    let Some(fuscript) = CurrentFileInfo::get_fuscript() else { return };
    let script = "c = Resolve():GetProjectManager():GetCurrentProject():GetCurrentTimeline():GetCurrentVideoItem();
                      c:SetProperty('FlipX', c:GetProperty('FlipX'))";
    let cmd = std::process::Command::new(&fuscript);
    #[cfg(target_os = "windows")]
    let cmd = { let mut cmd = cmd; use std::os::windows::process::CommandExt; cmd.creation_flags(0x08000000); cmd }; // CREATE_NO_WINDOW
    let mut cmd = cmd;
    let _guard = FUSCRIPT_LOCK.lock();
    let _ = cmd.args(["-q", "-l", "lua", "-x", script]).output();
}

/// Source-frame ranges `(start, end)` of every timeline item that plays a file.
/// Unlike `CurrentFileInfo`, this doesn't depend on the playhead.
pub struct SourceRanges;

impl SourceRanges {
    /// Lists all video items of the current timeline as `start<TAB>end<TAB>file path` lines.
    const SCRIPT: &'static str = "local tl = Resolve():GetProjectManager():GetCurrentProject():GetCurrentTimeline();
        for t = 1, tl:GetTrackCount('video') do
            local items = tl:GetItemListInTrack('video', t);
            if items then for k, v in pairs(items) do
                local it = (type(v) == 'number') and k or v;
                local ok_m, mp = pcall(function() return it:GetMediaPoolItem() end);
                local ok_s, ss = pcall(function() return it:GetSourceStartFrame() end);
                local ok_e, se = pcall(function() return it:GetSourceEndFrame() end);
                if ok_m and mp and ok_s and ok_e and ss and se then print(ss .. '\\t' .. se .. '\\t' .. (mp:GetClipProperty('File Path') or '')) end;
            end end;
        end;
        print('END');";

    /// Runs the query in the background and stores the ranges for `video_path` (empty if none match).
    /// `rerender`: make Resolve render the current item again once the answer is in.
    pub fn query(video_path: String, result: Arc<Mutex<Option<Vec<(f64, f64)>>>>, rerender: bool) {
        // Resolve keeps several instances per clip; they all ask at once, so share one recent answer
        static RECENT: Mutex<Option<(std::time::Instant, String)>> = Mutex::new(None);
        std::thread::spawn(move || {
            let Some(fuscript) = CurrentFileInfo::get_fuscript() else { return };
            for attempt in 0..3 {
                if attempt > 0 { std::thread::sleep(std::time::Duration::from_millis(1500)); }
                let stdout = {
                    let _guard = FUSCRIPT_LOCK.lock();
                    let recent = RECENT.lock().as_ref().filter(|(t, _)| t.elapsed().as_secs_f64() < 2.0).map(|(_, s)| s.clone());
                    match recent {
                        Some(stdout) => stdout,
                        None => {
                            let Ok(out) = std::process::Command::new(&fuscript).args(["-q", "-l", "lua", "-x", Self::SCRIPT]).output() else { return };
                            let stdout = String::from_utf8_lossy(&out.stdout).to_string();
                            if !stdout.lines().any(|l| l.trim() == "END") {
                                log::debug!("Source range query attempt {attempt} failed: {}", String::from_utf8_lossy(&out.stderr).trim());
                                continue;
                            }
                            *RECENT.lock() = Some((std::time::Instant::now(), stdout.clone()));
                            stdout
                        }
                    }
                };
                let ranges = Self::parse(&stdout, &video_path);
                log::info!("Timeline items using {video_path}: {ranges:?}");
                *result.lock() = Some(ranges);
                if rerender {
                    trigger_render();
                }
                return;
            }
        });
    }

    fn parse(stdout: &str, video_path: &str) -> Vec<(f64, f64)> {
        let file_name = |p: &str| std::path::Path::new(p).file_name().map(|x| x.to_string_lossy().to_lowercase());
        let items: Vec<(f64, f64, String)> = stdout.lines().filter_map(|line| {
            let mut parts = line.splitn(3, '\t');
            let start = parts.next()?.trim().parse::<f64>().ok()?;
            let end = parts.next()?.trim().parse::<f64>().ok()?;
            let path = replace_frame_count(parts.next()?.trim());
            (end > start).then_some((start, end, path))
        }).collect();
        let exact: Vec<_> = items.iter().filter(|x| x.2 == video_path).map(|x| (x.0, x.1)).collect();
        if !exact.is_empty() { return exact; }
        // Paths can differ in form (symlinks, cloud storage), so fall back to the file name
        let name = file_name(video_path);
        items.iter().filter(|x| name.is_some() && file_name(&x.2) == name).map(|x| (x.0, x.1)).collect()
    }

    /// The range that contains `src_frame` (within a few frames, as Resolve's instances
    /// can disagree by 1-2 frames), preferring the shortest one when cuts overlap.
    pub fn containing(ranges: &[(f64, f64)], src_frame: f64) -> Option<(f64, f64)> {
        let shortest = |tolerance: f64| ranges.iter().copied()
            .filter(|(s, e)| src_frame >= s - tolerance && src_frame < e + tolerance)
            .min_by(|a, b| (a.1 - a.0).total_cmp(&(b.1 - b.0)));
        shortest(0.0).or_else(|| shortest(3.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ranges_for_matching_file() {
        let out = "2118\t3620\t/a/GX010636.MP4\n0\t100\t/a/OTHER.MP4\n10\t20\t/a/GX010636.MP4\nEND\n";
        assert_eq!(SourceRanges::parse(out, "/a/GX010636.MP4"), vec![(2118.0, 3620.0), (10.0, 20.0)]);
    }

    #[test]
    fn falls_back_to_file_name() {
        let out = "5\t50\t/Volumes/x/gx010636.mp4\nEND\n";
        assert_eq!(SourceRanges::parse(out, "/Users/me/GX010636.MP4"), vec![(5.0, 50.0)]);
        assert!(SourceRanges::parse(out, "/Users/me/OTHER.MP4").is_empty());
    }

    #[test]
    fn picks_range_containing_frame() {
        let ranges = [(0.0, 1000.0), (2118.0, 3620.0), (2500.0, 2600.0)];
        assert_eq!(SourceRanges::containing(&ranges, 2200.0), Some((2118.0, 3620.0)));
        assert_eq!(SourceRanges::containing(&ranges, 2550.0), Some((2500.0, 2600.0)));
        assert_eq!(SourceRanges::containing(&ranges, 1500.0), None);
        // Split clip: the boundary frame belongs to the cut that starts there
        let split = [(0.0, 100.0), (100.0, 150.0)];
        assert_eq!(SourceRanges::containing(&split, 99.0), Some((0.0, 100.0)));
        assert_eq!(SourceRanges::containing(&split, 100.0), Some((100.0, 150.0)));
        assert_eq!(SourceRanges::containing(&split, 151.0), Some((100.0, 150.0)));
    }
    #[test]
    fn resolves_explicit_and_inherited_fit() {
        assert_eq!(ResolveScaling::from_values(Some(2), Some("scaleToFill")), ResolveScaling::Fit);
        assert_eq!(ResolveScaling::from_values(Some(3), Some("scaleToFit")), ResolveScaling::Other);
        assert_eq!(ResolveScaling::from_values(Some(0), Some("scaleToFit")), ResolveScaling::Fit);
        assert_eq!(ResolveScaling::from_values(Some(0), Some("scaleToFill")), ResolveScaling::Other);
    }
}
