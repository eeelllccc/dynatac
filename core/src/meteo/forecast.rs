use std::fmt;
use std::io::{self, Read};

use serde::Deserialize;

use super::Location;

pub const MAX_RESPONSE_BYTES: usize = 256 * 1024;
pub const FORECAST_DAYS: u8 = 7;
const MAX_HOURS: usize = 192;
const PACKAGES: &str = "basic-1h,wind-1h,profileclouds-1h,profileheight-1h";

#[derive(Debug, Clone)]
pub struct ForecastRequest {
    location: Location,
}

impl ForecastRequest {
    pub fn new(location: Location) -> Self {
        Self { location }
    }
    pub fn location(&self) -> &Location {
        &self.location
    }

    /// Construct the HTTPS URL. Callers must never log the returned value.
    pub fn url(&self, api_key: &str) -> Result<String, ForecastError> {
        if !(8..=128).contains(&api_key.len())
            || !api_key
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
        {
            return Err(ForecastError::InvalidApiKey);
        }
        let timezone = self
            .location
            .timezone()
            .replace('/', "%2F")
            .replace('+', "%2B");
        let altitude = self
            .location
            .altitude_metres()
            .map(|value| format!("&asl={value}"))
            .unwrap_or_default();
        Ok(format!(
            "https://my.meteoblue.com/packages/{PACKAGES}?lat={}&lon={}{}&forecastDays={FORECAST_DAYS}&tz={timezone}&timeformat=iso8601&format=json&temperatureUnit=C&windSpeedUnit=m%2Fs&windDirectionUnit=degree&precipitationUnit=metric&apikey={api_key}",
            self.location.latitude(), self.location.longitude(), altitude,
        ))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForecastError {
    InvalidApiKey,
    ResponseTooLarge,
    Io,
    InvalidJson,
    MissingField(&'static str),
    InvalidMetadata(&'static str),
    InvalidTimestamp(usize),
    InvalidSeriesLength(&'static str),
    InvalidValue(&'static str, usize),
}

impl fmt::Display for ForecastError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidApiKey => f.write_str("invalid Meteoblue API key"),
            Self::ResponseTooLarge => f.write_str("Meteoblue response exceeded 256 KiB"),
            Self::Io => f.write_str("could not read Meteoblue response"),
            Self::InvalidJson => f.write_str("Meteoblue returned invalid JSON"),
            Self::MissingField(field) => write!(f, "Meteoblue response is missing {field}"),
            Self::InvalidMetadata(field) => {
                write!(f, "Meteoblue returned invalid {field} metadata")
            }
            Self::InvalidTimestamp(index) => {
                write!(f, "invalid forecast timestamp at hour {index}")
            }
            Self::InvalidSeriesLength(field) => {
                write!(f, "forecast field {field} has the wrong length")
            }
            Self::InvalidValue(field, index) => write!(
                f,
                "forecast field {field} has an invalid value at hour {index}"
            ),
        }
    }
}

impl std::error::Error for ForecastError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LocalHour {
    pub year: i32,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub weekday: u8, // Monday = 0
    absolute_minute: i64,
}

#[derive(Debug)]
pub struct Forecast {
    pub(crate) location_name: String,
    pub(crate) ground_metres: f32,
    pub(crate) times: Vec<LocalHour>,
    pub(crate) temperature: Vec<f32>,
    pub(crate) pictocode: Vec<i16>,
    pub(crate) precipitation: Vec<f32>,
    pub(crate) snowfraction: Vec<f32>,
    pub(crate) windspeed: Vec<f32>,
    pub(crate) winddirection: Vec<f32>,
    pub(crate) gust: Vec<f32>,
    pub(crate) cloud: [Option<Vec<f32>>; 21],
    pub(crate) height: [Vec<f32>; 18],
}

impl Forecast {
    pub fn hour_count(&self) -> usize {
        self.times.len()
    }
    pub fn ground_metres(&self) -> f32 {
        self.ground_metres
    }
}

#[derive(Deserialize)]
struct Document {
    metadata: Metadata,
    data_1h: RawHourly,
}

#[derive(Deserialize)]
struct Metadata {
    height: f32,
    latitude: f64,
    longitude: f64,
}

#[derive(Deserialize)]
struct RawHourly {
    time: Vec<String>,
    temperature: Vec<f32>,
    pictocode: Vec<i16>,
    precipitation: Vec<f32>,
    snowfraction: Vec<f32>,
    windspeed: Vec<f32>,
    winddirection: Vec<f32>,
    gust: Vec<f32>,

    cloudcoverprofile_1000_mb: Vec<f32>,
    #[serde(default)]
    cloudcoverprofile_975_mb: Option<Vec<f32>>,
    cloudcoverprofile_950_mb: Vec<f32>,
    #[serde(default)]
    cloudcoverprofile_925_mb: Option<Vec<f32>>,
    cloudcoverprofile_900_mb: Vec<f32>,
    #[serde(default)]
    cloudcoverprofile_875_mb: Option<Vec<f32>>,
    cloudcoverprofile_850_mb: Vec<f32>,
    cloudcoverprofile_800_mb: Vec<f32>,
    cloudcoverprofile_750_mb: Vec<f32>,
    cloudcoverprofile_700_mb: Vec<f32>,
    cloudcoverprofile_650_mb: Vec<f32>,
    cloudcoverprofile_600_mb: Vec<f32>,
    cloudcoverprofile_550_mb: Vec<f32>,
    cloudcoverprofile_500_mb: Vec<f32>,
    cloudcoverprofile_450_mb: Vec<f32>,
    cloudcoverprofile_400_mb: Vec<f32>,
    cloudcoverprofile_350_mb: Vec<f32>,
    cloudcoverprofile_300_mb: Vec<f32>,
    cloudcoverprofile_250_mb: Vec<f32>,
    cloudcoverprofile_200_mb: Vec<f32>,
    cloudcoverprofile_150_mb: Vec<f32>,

    heightprofile_1000_mb: Vec<f32>,
    heightprofile_950_mb: Vec<f32>,
    heightprofile_900_mb: Vec<f32>,
    heightprofile_850_mb: Vec<f32>,
    heightprofile_800_mb: Vec<f32>,
    heightprofile_750_mb: Vec<f32>,
    heightprofile_700_mb: Vec<f32>,
    heightprofile_650_mb: Vec<f32>,
    heightprofile_600_mb: Vec<f32>,
    heightprofile_550_mb: Vec<f32>,
    heightprofile_500_mb: Vec<f32>,
    heightprofile_450_mb: Vec<f32>,
    heightprofile_400_mb: Vec<f32>,
    heightprofile_350_mb: Vec<f32>,
    heightprofile_300_mb: Vec<f32>,
    heightprofile_250_mb: Vec<f32>,
    heightprofile_200_mb: Vec<f32>,
    heightprofile_150_mb: Vec<f32>,
}

struct CountingReader<R> {
    inner: R,
    count: usize,
}

impl<R: Read> Read for CountingReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.count > MAX_RESPONSE_BYTES {
            return Ok(0);
        }
        let remaining = MAX_RESPONSE_BYTES + 1 - self.count;
        let length = buffer.len().min(remaining);
        let read = self.inner.read(&mut buffer[..length])?;
        self.count += read;
        Ok(read)
    }
}

/// Decode and validate a bounded API response without first allocating its body.
pub fn decode_forecast<R: Read>(
    reader: R,
    request: &ForecastRequest,
) -> Result<Forecast, ForecastError> {
    let mut reader = CountingReader {
        inner: reader,
        count: 0,
    };
    let result: Result<Document, _> = serde_json::from_reader(&mut reader);
    if reader.count > MAX_RESPONSE_BYTES {
        return Err(ForecastError::ResponseTooLarge);
    }
    let document = result.map_err(|error| {
        if error.is_io() {
            ForecastError::Io
        } else {
            ForecastError::InvalidJson
        }
    })?;
    validate(document, request.location())
}

fn validate(document: Document, location: &Location) -> Result<Forecast, ForecastError> {
    if !document.metadata.height.is_finite()
        || !(-500.0..=9000.0).contains(&document.metadata.height)
    {
        return Err(ForecastError::InvalidMetadata("height"));
    }
    if (document.metadata.latitude - location.latitude()).abs() > 0.000_1 {
        return Err(ForecastError::InvalidMetadata("latitude"));
    }
    if (document.metadata.longitude - location.longitude()).abs() > 0.000_1 {
        return Err(ForecastError::InvalidMetadata("longitude"));
    }
    if let Some(expected) = location.altitude_metres() {
        if (document.metadata.height - expected as f32).abs() > 0.5 {
            return Err(ForecastError::InvalidMetadata("height"));
        }
    }

    let raw = document.data_1h;
    let count = raw.time.len();
    if !(2..=MAX_HOURS).contains(&count) {
        return Err(ForecastError::InvalidSeriesLength("time"));
    }
    let mut times = Vec::with_capacity(count);
    for (index, text) in raw.time.iter().enumerate() {
        let parsed = parse_iso_hour(text).ok_or(ForecastError::InvalidTimestamp(index))?;
        if times.last().is_some_and(|previous: &LocalHour| {
            parsed.absolute_minute != previous.absolute_minute + 60
        }) {
            return Err(ForecastError::InvalidTimestamp(index));
        }
        times.push(parsed);
    }

    macro_rules! length {
        ($field:ident) => {
            if raw.$field.len() != count {
                return Err(ForecastError::InvalidSeriesLength(stringify!($field)));
            }
        };
    }
    length!(temperature);
    length!(pictocode);
    length!(precipitation);
    length!(snowfraction);
    length!(windspeed);
    length!(winddirection);
    length!(gust);
    length!(cloudcoverprofile_1000_mb);
    length!(cloudcoverprofile_950_mb);
    length!(cloudcoverprofile_900_mb);
    length!(cloudcoverprofile_850_mb);
    length!(cloudcoverprofile_800_mb);
    length!(cloudcoverprofile_750_mb);
    length!(cloudcoverprofile_700_mb);
    length!(cloudcoverprofile_650_mb);
    length!(cloudcoverprofile_600_mb);
    length!(cloudcoverprofile_550_mb);
    length!(cloudcoverprofile_500_mb);
    length!(cloudcoverprofile_450_mb);
    length!(cloudcoverprofile_400_mb);
    length!(cloudcoverprofile_350_mb);
    length!(cloudcoverprofile_300_mb);
    length!(cloudcoverprofile_250_mb);
    length!(cloudcoverprofile_200_mb);
    length!(cloudcoverprofile_150_mb);
    length!(heightprofile_1000_mb);
    length!(heightprofile_950_mb);
    length!(heightprofile_900_mb);
    length!(heightprofile_850_mb);
    length!(heightprofile_800_mb);
    length!(heightprofile_750_mb);
    length!(heightprofile_700_mb);
    length!(heightprofile_650_mb);
    length!(heightprofile_600_mb);
    length!(heightprofile_550_mb);
    length!(heightprofile_500_mb);
    length!(heightprofile_450_mb);
    length!(heightprofile_400_mb);
    length!(heightprofile_350_mb);
    length!(heightprofile_300_mb);
    length!(heightprofile_250_mb);
    length!(heightprofile_200_mb);
    length!(heightprofile_150_mb);

    for (name, optional) in [
        ("cloudcoverprofile_975_mb", &raw.cloudcoverprofile_975_mb),
        ("cloudcoverprofile_925_mb", &raw.cloudcoverprofile_925_mb),
        ("cloudcoverprofile_875_mb", &raw.cloudcoverprofile_875_mb),
    ] {
        if optional
            .as_ref()
            .is_some_and(|values| values.len() != count)
        {
            return Err(ForecastError::InvalidSeriesLength(name));
        }
    }

    fn finite_range(
        values: &[f32],
        name: &'static str,
        low: f32,
        high: f32,
    ) -> Result<(), ForecastError> {
        for (index, value) in values.iter().enumerate() {
            if !value.is_finite() || !(*value >= low && *value <= high) {
                return Err(ForecastError::InvalidValue(name, index));
            }
        }
        Ok(())
    }
    finite_range(&raw.temperature, "temperature", -100.0, 70.0)?;
    finite_range(&raw.precipitation, "precipitation", 0.0, 1000.0)?;
    finite_range(&raw.snowfraction, "snowfraction", 0.0, 1.0)?;
    finite_range(&raw.windspeed, "windspeed", 0.0, 200.0)?;
    finite_range(&raw.gust, "gust", 0.0, 200.0)?;
    finite_range(&raw.winddirection, "winddirection", 0.0, 360.0)?;
    for (index, code) in raw.pictocode.iter().enumerate() {
        if !(0..=40).contains(code) {
            return Err(ForecastError::InvalidValue("pictocode", index));
        }
    }

    let cloud = [
        Some(raw.cloudcoverprofile_1000_mb),
        raw.cloudcoverprofile_975_mb,
        Some(raw.cloudcoverprofile_950_mb),
        raw.cloudcoverprofile_925_mb,
        Some(raw.cloudcoverprofile_900_mb),
        raw.cloudcoverprofile_875_mb,
        Some(raw.cloudcoverprofile_850_mb),
        Some(raw.cloudcoverprofile_800_mb),
        Some(raw.cloudcoverprofile_750_mb),
        Some(raw.cloudcoverprofile_700_mb),
        Some(raw.cloudcoverprofile_650_mb),
        Some(raw.cloudcoverprofile_600_mb),
        Some(raw.cloudcoverprofile_550_mb),
        Some(raw.cloudcoverprofile_500_mb),
        Some(raw.cloudcoverprofile_450_mb),
        Some(raw.cloudcoverprofile_400_mb),
        Some(raw.cloudcoverprofile_350_mb),
        Some(raw.cloudcoverprofile_300_mb),
        Some(raw.cloudcoverprofile_250_mb),
        Some(raw.cloudcoverprofile_200_mb),
        Some(raw.cloudcoverprofile_150_mb),
    ];
    for values in cloud.iter().flatten() {
        finite_range(values, "cloud cover", 0.0, 100.0)?;
    }
    let height = [
        raw.heightprofile_1000_mb,
        raw.heightprofile_950_mb,
        raw.heightprofile_900_mb,
        raw.heightprofile_850_mb,
        raw.heightprofile_800_mb,
        raw.heightprofile_750_mb,
        raw.heightprofile_700_mb,
        raw.heightprofile_650_mb,
        raw.heightprofile_600_mb,
        raw.heightprofile_550_mb,
        raw.heightprofile_500_mb,
        raw.heightprofile_450_mb,
        raw.heightprofile_400_mb,
        raw.heightprofile_350_mb,
        raw.heightprofile_300_mb,
        raw.heightprofile_250_mb,
        raw.heightprofile_200_mb,
        raw.heightprofile_150_mb,
    ];
    for values in &height {
        finite_range(values, "profile height", -1000.0, 30_000.0)?;
    }

    Ok(Forecast {
        location_name: location.display_name().to_string(),
        ground_metres: document.metadata.height,
        times,
        temperature: raw.temperature,
        pictocode: raw.pictocode,
        precipitation: raw.precipitation,
        snowfraction: raw.snowfraction,
        windspeed: raw.windspeed,
        winddirection: raw.winddirection,
        gust: raw.gust,
        cloud,
        height,
    })
}

fn parse_iso_hour(text: &str) -> Option<LocalHour> {
    let (date, time_and_offset) = text.split_once('T')?;
    let mut date_parts = date.split('-');
    let year: i32 = date_parts.next()?.parse().ok()?;
    let month: u8 = date_parts.next()?.parse().ok()?;
    let day: u8 = date_parts.next()?.parse().ok()?;
    if date_parts.next().is_some()
        || !(1..=12).contains(&month)
        || day == 0
        || day > days_in_month(year, month)
    {
        return None;
    }

    let offset_index = time_and_offset
        .char_indices()
        .skip(1)
        .find_map(|(index, ch)| matches!(ch, '+' | '-').then_some(index));
    let (clock, offset_minutes) = if let Some(index) = offset_index {
        let clock = &time_and_offset[..index];
        let offset = &time_and_offset[index..];
        let sign = if offset.starts_with('-') { -1 } else { 1 };
        let mut parts = offset[1..].split(':');
        let hours: i64 = parts.next()?.parse().ok()?;
        let minutes: i64 = parts.next()?.parse().ok()?;
        if parts.next().is_some() || hours > 23 || minutes > 59 {
            return None;
        }
        (clock, sign * (hours * 60 + minutes))
    } else if let Some(clock) = time_and_offset.strip_suffix('Z') {
        (clock, 0)
    } else {
        return None;
    };
    let mut clock_parts = clock.split(':');
    let hour: u8 = clock_parts.next()?.parse().ok()?;
    let minute: u8 = clock_parts.next()?.parse().ok()?;
    let second = clock_parts
        .next()
        .map(|part| part.parse::<u8>().ok())
        .flatten()
        .unwrap_or(0);
    if clock_parts.next().is_some() || hour > 23 || minute != 0 || second != 0 {
        return None;
    }

    let days = days_from_civil(year, month, day);
    let absolute_minutes = days * 1440 + hour as i64 * 60 - offset_minutes;
    // 1970-01-01 was Thursday; Monday is zero.
    let weekday = (days + 3).rem_euclid(7) as u8;
    Some(LocalHour {
        year,
        month,
        day,
        hour,
        weekday,
        absolute_minute: absolute_minutes,
    })
}

fn days_in_month(year: i32, month: u8) -> u8 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        _ => 0,
    }
}

// Howard Hinnant's civil-date conversion, returning days relative to 1970-01-01.
fn days_from_civil(year: i32, month: u8, day: u8) -> i64 {
    let year = year - i32::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = year - era * 400;
    let adjusted_month = month as i32 + if month > 2 { -3 } else { 9 };
    let doy = (153 * adjusted_month + 2) / 5 + day as i32 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    (era * 146_097 + doe - 719_468) as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meteo::default_location;

    #[test]
    fn request_omits_leith_altitude_and_daily_package() {
        let request = ForecastRequest::new(default_location());
        let url = request.url("example_key_123").unwrap();
        assert!(url.contains("lat=55.97&lon=-3.17&forecastDays=7"));
        assert!(url.contains("basic-1h,wind-1h,profileclouds-1h,profileheight-1h"));
        assert!(!url.contains("basic-day"));
        assert!(!url.contains("asl="));
    }

    #[test]
    fn request_includes_an_explicit_location_altitude() {
        let location = Location::new(
            "summit",
            "Summit",
            57.09,
            -3.65,
            Some(1047),
            "Europe/London",
        )
        .unwrap();
        let url = ForecastRequest::new(location)
            .url("example_key_123")
            .unwrap();
        assert!(url.contains("lat=57.09&lon=-3.65&asl=1047&forecastDays=7"));
    }

    #[test]
    fn timestamps_handle_offset_changes_as_real_hours() {
        let before = parse_iso_hour("2026-10-25T01:00+01:00").unwrap();
        let after = parse_iso_hour("2026-10-25T01:00+00:00").unwrap();
        assert_eq!(after.absolute_minute, before.absolute_minute + 60);
        assert_eq!(before.weekday, 6);
    }
}
