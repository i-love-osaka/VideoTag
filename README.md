# VideoTag

If MusicTag is for songs, VideoTag is for the video shelf. Point it at a
folder of downloaded movies and episodes, and it works out what each
file is, looks it up on TMDB, TheTVDB, or Bangumi, and renames
everything into one consistent library layout:

```text
[8.4] [2019] [TMDB-00082856]《曼达洛人》(The Mandalorian)
  /Season 01
  /曼达洛人 - S01E01 - 曼达洛人 (2019-11-12)-2160p-HEVC-10bit-TrueHD 8ch 7.1.mkv
```

## How it works

- Drop in a folder. The show name is read from the folder name —
  including names in 《》, names split by dashes, and folders that
  already carry an ID, like `101次求婚-TMDB48455`. Episode numbers come
  from `S01E01`, `第1集`, or plain numbered files (`01`, `02`, …) that
  belong to one show.
- Match on the platform you pick: TMDB, TheTVDB, or Bangumi. It never
  silently falls back to another platform. A folder carrying that
  platform's ID goes straight to the record; otherwise it searches by
  title.
- Wrong match? Open the candidate list and pick another. A file it
  can't recognize at all can be rescued by typing the ID yourself.
- Episode titles and air dates come from the database. The quality
  tags at the end of a filename (2160p, HEVC, 10bit, TrueHD 8ch 7.1…)
  are read by probing the actual file with FFmpeg, not guessed from
  the filename.
- Subtitles travel with their video and keep language suffixes like
  `.chs`. Small extra files are flagged so you can decide about them.
- Conflicts are painted red before anything runs, and conflicting
  files are skipped. It never overwrites an existing file.
- Save in place, or to another drive or NAS. On the same volume a
  move is a plain rename; across volumes the copy is size-checked
  before the source is deleted.

## API keys

Matching needs API keys, and they are yours to bring: a free TMDB key
(required), a TheTVDB project key (optional), and Bangumi needs none.
Keys live only in the app's settings file on your own machine.

## Download

Prebuilt macOS builds are on the
[Releases](https://github.com/i-love-osaka/VideoTag/releases) page.

## Build

Requires the stable Rust toolchain; on macOS, the Xcode Command Line
Tools for the app bundle. FFmpeg is bundled with the release app for
media probing.

```sh
cargo build --release -p videotag-app
bash packaging/macos/build-app.sh
# or double-click 编译Mac版.command in Finder
```

The interface is Simplified Chinese and English, following your system
language. Currently macOS first (Apple Silicon); the core libraries are
platform-independent.

## 中文简介

VideoTag 是「视频版的 MusicTag」：从 TMDB、TheTVDB、Bangumi 匹配
电影和剧集信息，按统一格式重命名文件与目录。

- 剧名从目录名提取：读得懂《》里的名字、破折号分隔的名字，也认
  `101次求婚-TMDB48455` 这种自带平台 ID 的目录；集数认 `S01E01`、
  `第1集`，也认一整季纯数字命名的文件
- 在哪个平台匹配由你点哪个按钮决定，不跨平台乱兜底；匹配错了
  打开候选列表换一个，实在识别不了的可以手动指定 ID 救回来
- 集标题、播出日期取自数据库；文件名尾巴上的画质标记（2160p、
  HEVC、10bit、TrueHD 8ch 7.1……）是拿内置 FFmpeg 真探测出来的，
  不是猜文件名
- 字幕跟着视频走，保留 `.chs` 这类语言后缀；重名冲突提前标红、
  执行时跳过，绝不覆盖已有文件
- 可以原地改名，也可以存到外置盘或 NAS：同盘是服务器端改名，
  跨盘拷贝校验大小之后才删源文件

需要自备 API 密钥：TMDB 免费密钥（必需）、TheTVDB 项目密钥
（可选），Bangumi 不需要。密钥只存在本机设置里。

Metadata courtesy of TMDB, TheTVDB, and Bangumi. This product uses
their data but is not endorsed by them.

## License

MIT © Ben Zhang

By [Mr.Ben](https://mrben.net)
