use std::fmt;
use std::str::FromStr;

/// 单元格坐标与值（完全独立，不依赖 calamine）
#[derive(Debug, Clone, PartialEq)]
pub struct Cell<T> {
    pos: (u32, u32),
    val: T,
}

impl<T> Cell<T> {
    pub fn new(position: (u32, u32), value: T) -> Self {
        Self {
            pos: position,
            val: value,
        }
    }
    pub fn get_position(&self) -> (u32, u32) {
        self.pos
    }
    pub fn get_value(&self) -> &T {
        &self.val
    }
    pub fn into_value(self) -> T {
        self.val
    }
}

impl<T> From<Cell<T>> for String
where
    T: Into<String>,
{
    fn from(value: Cell<T>) -> Self {
        value.into_value().into()
    }
}

/// Excel 单元格数据类型（完全独立）
#[derive(Debug, Clone, PartialEq, Default)]
pub enum Data {
    Int(i64),
    Float(f64),
    String(String),
    Bool(bool),
    DateTime(ExcelDateTime),
    DateTimeIso(String),
    DurationIso(String),
    Error(CellErrorType),
    #[default]
    Empty,
}

impl fmt::Display for Data {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Data::Int(e) => write!(f, "{e}"),
            Data::Float(e) => write!(f, "{e}"),
            Data::String(e) => write!(f, "{e}"),
            Data::Bool(e) => write!(f, "{e}"),
            Data::DateTime(e) => write!(f, "{e}"),
            Data::DateTimeIso(e) => write!(f, "{e}"),
            Data::DurationIso(e) => write!(f, "{e}"),
            Data::Error(e) => write!(f, "{e}"),
            Data::Empty => Ok(()),
        }
    }
}

impl From<Data> for String {
    fn from(data: Data) -> String {
        match data {
            Data::String(s) => s, // 直接移出，零分配
            Data::Int(i) => i.to_string(),
            Data::Float(f) => f.to_string(),
            Data::Bool(b) => b.to_string(),
            Data::DateTime(dt) => dt.to_string(),
            Data::DateTimeIso(s) | Data::DurationIso(s) => s, // 同样移出
            Data::Error(e) => e.to_string(),
            Data::Empty => String::new(),
        }
    }
}

/// 单元格错误类型
#[derive(Debug, Clone, PartialEq)]
pub enum CellErrorType {
    Div0,
    NA,
    Name,
    Null,
    Num,
    Ref,
    Value,
    GettingData,
}

impl fmt::Display for CellErrorType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            CellErrorType::Div0 => write!(f, "#DIV/0!"),
            CellErrorType::NA => write!(f, "#N/A"),
            CellErrorType::Name => write!(f, "#NAME?"),
            CellErrorType::Null => write!(f, "#NULL!"),
            CellErrorType::Num => write!(f, "#NUM!"),
            CellErrorType::Ref => write!(f, "#REF!"),
            CellErrorType::Value => write!(f, "#VALUE!"),
            CellErrorType::GettingData => write!(f, "#DATA!"),
        }
    }
}

impl FromStr for CellErrorType {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "#DIV/0!" => Ok(CellErrorType::Div0),
            "#N/A" => Ok(CellErrorType::NA),
            "#NAME?" => Ok(CellErrorType::Name),
            "#NULL!" => Ok(CellErrorType::Null),
            "#NUM!" => Ok(CellErrorType::Num),
            "#REF!" => Ok(CellErrorType::Ref),
            "#VALUE!" => Ok(CellErrorType::Value),
            "#DATA!" => Ok(CellErrorType::GettingData),
            _ => Err(()),
        }
    }
}

/// Excel 日期时间表示（从 calamine 移植的核心转换算法）
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ExcelDateTime {
    value: f64,
    is_1904: bool,
}

// 转换算法常量
const DAY_SECONDS: f64 = 24.0 * 60.0 * 60.;
const HOUR_SECONDS: u64 = 60 * 60;
const MINUTE_SECONDS: u64 = 60;
const YEAR_DAYS: u64 = 365;
const YEAR_DAYS_4: u64 = YEAR_DAYS * 4 + 1;
const YEAR_DAYS_100: u64 = YEAR_DAYS * 100 + 25;
const YEAR_DAYS_400: u64 = YEAR_DAYS * 400 + 97;

impl ExcelDateTime {
    pub fn new(value: f64, is_1904: bool) -> Self {
        Self { value, is_1904 }
    }

    pub fn as_f64(&self) -> f64 {
        self.value
    }

    /// 直接返回纳秒时间戳（跳过 chrono 双重校验）
    ///
    /// Excel 1900 calendar 伪 epoch 为 1899-12-30（兼容 1900 闰年 bug）。
    /// 当 days >= 61 时标准历法与 Excel 一致；days 1..=59 时 Excel 比标准快 1 天。
    pub fn to_timestamp_nanos(&self) -> i64 {
        const NANOS_PER_DAY: i64 = 86_400_000_000_000i64;

        let days = self.value.floor() as i64;
        let fract = self.value.fract();

        // 计算自 Unix epoch (1970-01-01) 以来的天数
        let unix_days = if self.is_1904 {
            days + 24_107 // 1904-01-01 → 1970-01-01 = 24107 天
        } else if days > 60 {
            days - 25_569 // 1899-12-30 → 1970-01-01 = 25569 天
        } else if days >= 1 {
            days - 25_568 // 补偿 Excel 1900 闰年 bug（1..=59 快 1 天）
        } else {
            days - 25_569
        };

        // 时间部分：毫秒精度已足够（Excel 只存到毫秒）
        let time_millis = (fract * 86_400_000f64).round() as i64;

        unix_days * NANOS_PER_DAY + time_millis * 1_000_000
    }

    pub fn to_ymd_hms_milli(&self) -> (u16, u8, u8, u8, u8, u8, u16) {
        let mut months = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];

        let mut days = self.value.floor() as u64;

        if self.is_1904 {
            days += 111_033;
        } else if days > YEAR_DAYS {
            days += 109_571;
        } else {
            days += 109_572;
        }

        let year_days_400 = days / YEAR_DAYS_400;
        let mut days = days % YEAR_DAYS_400;

        let year_days_100;
        if days < YEAR_DAYS_100 {
            year_days_100 = days / YEAR_DAYS_100;
            days %= YEAR_DAYS_100;
        } else {
            year_days_100 = 1 + (days - YEAR_DAYS_100) / (YEAR_DAYS_100 - 1);
            days = (days - YEAR_DAYS_100) % (YEAR_DAYS_100 - 1);
        }

        let year_days_4;
        let mut non_leap_year_block = false;
        if year_days_100 == 0 {
            year_days_4 = days / YEAR_DAYS_4;
            days %= YEAR_DAYS_4;
        } else if days < YEAR_DAYS_4 {
            year_days_4 = days / (YEAR_DAYS_4 - 1);
            days %= YEAR_DAYS_4 - 1;
            non_leap_year_block = true;
        } else {
            year_days_4 = 1 + (days - (YEAR_DAYS_4 - 1)) / YEAR_DAYS_4;
            days = (days - (YEAR_DAYS_4 - 1)) % YEAR_DAYS_4;
        }

        let year_days_1;
        if non_leap_year_block {
            year_days_1 = days / YEAR_DAYS;
            days %= YEAR_DAYS;
        } else if days < YEAR_DAYS + 1 {
            year_days_1 = days / (YEAR_DAYS + 1);
            days %= YEAR_DAYS + 1;
        } else {
            year_days_1 = 1 + (days - (YEAR_DAYS + 1)) / YEAR_DAYS;
            days = (days - (YEAR_DAYS + 1)) % YEAR_DAYS;
        }

        let year = 1600 + year_days_400 * 400 + year_days_100 * 100 + year_days_4 * 4 + year_days_1;
        days += 1;

        if Self::is_leap_year(year) {
            months[1] = 29;
        }

        if !self.is_1904 && year == 1900 {
            months[1] = 29;
            if self.value.trunc() == 366.0 {
                days += 1;
            }
        }

        let mut month = 1;
        for month_days in months {
            if days > month_days {
                days -= month_days;
                month += 1;
            } else {
                break;
            }
        }
        let day = days;

        let time = self.value.fract();
        let mut milli = ((time * DAY_SECONDS).fract() * 1000.0).round() as u64;
        let mut day_as_seconds = (time * DAY_SECONDS) as u64;

        if milli == 1000 {
            day_as_seconds += 1;
            milli = 0;
        }

        let hour = day_as_seconds / HOUR_SECONDS;
        let min = (day_as_seconds - hour * HOUR_SECONDS) / MINUTE_SECONDS;
        let sec = (day_as_seconds - hour * HOUR_SECONDS - min * MINUTE_SECONDS) % MINUTE_SECONDS;

        (
            year as u16,
            month as u8,
            day as u8,
            hour as u8,
            min as u8,
            sec as u8,
            milli as u16,
        )
    }

    fn is_leap_year(year: u64) -> bool {
        year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
    }
}

impl Default for ExcelDateTime {
    fn default() -> Self {
        Self {
            value: 0.,
            is_1904: false,
        }
    }
}

impl fmt::Display for ExcelDateTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.value)
    }
}

/// 维度信息
#[derive(Debug, Default, PartialEq, Copy, Clone)]
pub struct Dimensions {
    pub start: (u32, u32),
    pub end: (u32, u32),
}
