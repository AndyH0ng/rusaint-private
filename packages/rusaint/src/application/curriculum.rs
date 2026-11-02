use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use wdpe::command::WebDynproCommandExecutor;
use wdpe::command::element::action::ButtonPressEventCommand;
use wdpe::command::element::selection::{
    ComboBoxItemListBoxCommand, ComboBoxSelectByValue1EventCommand, ComboBoxSelectEventCommand,
    ListBoxItemInfoCommand,
};
use wdpe::define_elements;
use wdpe::element::action::Button;
use wdpe::element::complex::SapTable;
use wdpe::element::definition::ElementDefinition as _;
use wdpe::element::parser::ElementParser;
use wdpe::element::selection::ComboBox;
use wdpe::element::selection::list_box::item::ListBoxItemInfo;
use wdpe::error::WebDynproError;
use wdpe::event::ucf_parameters::{UcfAction, UcfParametersBuilder, UcfResponseData};
use wdpe::event::{Event, EventBuilder};

use crate::client::{USaintApplication, USaintClient};
use crate::{ApplicationError, RusaintError};

/// [입학년도 별 교과과정 조회](https://ecc.ssu.ac.kr/sap/bc/webdynpro/SAP/ZCMA6W1010)
///
/// 입학년도·단과대학·학과(·전공)를 고르면 그 입학년도 학생에게 적용되는 교과과정을 보여 줍니다.
/// 2016학년도 이후 입학자만 조회할 수 있습니다.
#[derive(Debug)]
pub struct CurriculumApplication {
    client: USaintClient,
}

impl USaintApplication for CurriculumApplication {
    const APP_NAME: &'static str = "ZCMA6W1010";

    fn from_client(client: USaintClient) -> Result<Self, RusaintError> {
        if client.name() != Self::APP_NAME {
            Err(RusaintError::InvalidClientError)
        } else {
            Ok(Self { client })
        }
    }
}

/// 교과과정 한 과목
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct CurriculumCourse {
    /// 학년도
    pub year: String,
    /// 학기 (1학기, 2학기 …)
    pub semester: String,
    /// 권장 학년
    pub grade: String,
    /// 교과목 ID
    pub code: String,
    /// 국문 과목명
    pub name: String,
    /// 영문 과목명
    pub name_en: String,
    /// 이수구분 (전기, 전필, 전선 …)
    pub category: String,
    /// 학점
    pub credits: String,
    /// 시간
    pub hours: String,
    /// 국문 개요
    pub description: String,
    /// 영문 개요
    pub description_en: String,
}

/// 표가 한 번에 그리는 줄 수
const ROWS_PER_FRAGMENT: u32 = 20;

static ROW_COUNT: std::sync::LazyLock<regex_lite::Regex> =
    std::sync::LazyLock::new(|| regex_lite::Regex::new(r#"\browcount="(\d+)""#).unwrap());

/// 브라우저가 SapTableCS 를 스크롤할 때 보내는 RequestData 이벤트. `first`~`last` 줄을 요청합니다.
fn request_rows(table_id: &str, first: u32, last: u32) -> Result<Event, WebDynproError> {
    let parameters = HashMap::from([
        ("Id".to_string(), table_id.to_string()),
        ("DataFragments".to_string(), format!("{first},{last};")),
        ("FirstVisibleContentRowIndex".to_string(), first.to_string()),
        (
            "VisibleContentRowCount".to_string(),
            ROWS_PER_FRAGMENT.to_string(),
        ),
    ]);
    let ucf = UcfParametersBuilder::default()
        .response(Some(UcfResponseData::Delta))
        .action(Some(UcfAction::SubmitAsync))
        .build()
        .map_err(|e| {
            WebDynproError::from(wdpe::error::ElementError::InvalidContent {
                element: table_id.to_string(),
                content: e.to_string(),
            })
        })?;
    EventBuilder::default()
        .control("SapTableCS".to_string())
        .event("RequestData".to_string())
        .parameters(parameters)
        .ucf_parameters(ucf)
        .build()
        .map_err(|e| {
            WebDynproError::from(wdpe::error::ElementError::InvalidContent {
                element: table_id.to_string(),
                content: e.to_string(),
            })
        })
}

impl CurriculumCourse {
    fn same_row(&self, other: &Self) -> bool {
        (
            &self.code,
            &self.year,
            &self.semester,
            &self.grade,
            &self.category,
        ) == (
            &other.code,
            &other.year,
            &other.semester,
            &other.grade,
            &other.category,
        )
    }
}

static CELL: std::sync::LazyLock<regex_lite::Regex> = std::sync::LazyLock::new(|| {
    regex_lite::Regex::new(
        r#"id="ZCMA6W1010\.ID_0001:MAIN\.CT_DISPLAY_([A-Z_]+?)(?:_EDITOR|_LINK)\.(\d+)(?:-text)?"[^>]*>([^<]*)<"#,
    )
    .unwrap()
});

fn decode_entities(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let Some(end) = rest.find(';') else {
            break;
        };
        let entity = &rest[1..end];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some(' '),
            e if e.starts_with("#x") => u32::from_str_radix(&e[2..], 16)
                .ok()
                .and_then(char::from_u32),
            e if e.starts_with('#') => e[1..].parse().ok().and_then(char::from_u32),
            _ => None,
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// 지금 그려진 줄들을 칸 id(`CT_DISPLAY_필드.줄번호`)로 읽습니다.
fn parse_rows(html: &str) -> Vec<CurriculumCourse> {
    let mut rows: std::collections::BTreeMap<u32, HashMap<String, String>> = Default::default();
    for cap in CELL.captures_iter(html) {
        let (Some(field), Some(row), Some(text)) = (cap.get(1), cap.get(2), cap.get(3)) else {
            continue;
        };
        let Ok(row) = row.as_str().parse::<u32>() else {
            continue;
        };
        rows.entry(row).or_default().insert(
            field.as_str().to_string(),
            decode_entities(text.as_str()).trim().to_string(),
        );
    }
    rows.into_values()
        .filter_map(|cells| {
            let get = |key: &str| cells.get(key).cloned().unwrap_or_default();
            let code = get("SM_ID");
            (!code.is_empty()).then(|| CurriculumCourse {
                year: get("PERYR"),
                semester: get("PERIT"),
                grade: get("SM_LEVELT"),
                code,
                name: get("SM_STEXT_KO"),
                name_en: get("SM_STEXT_EN"),
                category: get("CG_GBT"),
                credits: get("CPOPT"),
                hours: get("PROZT"),
                description: get("KO_TEXT"),
                description_en: get("EN_TEXT"),
            })
        })
        .collect()
}

#[allow(unused)]
impl<'app> CurriculumApplication {
    define_elements! {
        YEAR: ComboBox<'app> = "ZCMA6W1010.ID_0001:MAIN.IF_PERYR";
        COLLAGE: ComboBox<'app> = "ZCMA6W1010.ID_0001:MAIN.IF_LV1_O";
        DEPARTMENT: ComboBox<'app> = "ZCMA6W1010.ID_0001:MAIN.IF_LV2_O";
        MAJOR: ComboBox<'app> = "ZCMA6W1010.ID_0001:MAIN.IF_LV3_O";
        SEARCH: Button<'app> = "ZCMA6W1010.ID_0001:MAIN.BU_SEARCH";
        TABLE: SapTable<'app> = "ZCMA6W1010.ID_0001:MAIN.CT_DISPLAY";
    }

    async fn select_year(&mut self, year: u32) -> Result<(), WebDynproError> {
        let event = ElementParser::new(self.client.body()).read(
            ComboBoxSelectEventCommand::new(Self::YEAR, &year.to_string(), false),
        )?;
        self.client.process_event(false, event).await?;
        Ok(())
    }

    async fn select(
        &mut self,
        combo: wdpe::element::selection::ComboBoxDef,
        value: &str,
    ) -> Result<(), WebDynproError> {
        let event = ElementParser::new(self.client.body())
            .read(ComboBoxSelectByValue1EventCommand::new(combo, value, false))?;
        self.client.process_event(false, event).await?;
        Ok(())
    }

    fn items(
        &self,
        combo: wdpe::element::selection::ComboBoxDef,
    ) -> Result<Vec<String>, WebDynproError> {
        let parser = ElementParser::new(self.client.body());
        let list = parser.read(ComboBoxItemListBoxCommand::new(combo))?;
        Ok(parser
            .read(ListBoxItemInfoCommand::new(list))?
            .into_iter()
            .filter_map(|info| match info {
                ListBoxItemInfo::Item { value1, .. } => Some(value1),
                ListBoxItemInfo::ActionItem { .. } => None,
            })
            .filter(|v| !v.trim().is_empty())
            .collect())
    }

    /// 입학년도에 고를 수 있는 단과대학
    pub async fn collages(&mut self, year: u32) -> Result<Vec<String>, RusaintError> {
        self.select_year(year).await?;
        Ok(self.items(Self::COLLAGE)?)
    }

    /// 단과대학의 학과
    pub async fn departments(
        &mut self,
        year: u32,
        collage: &str,
    ) -> Result<Vec<String>, RusaintError> {
        self.select_year(year).await?;
        self.select(Self::COLLAGE, collage).await?;
        Ok(self.items(Self::DEPARTMENT)?)
    }

    /// 학과의 전공 (전공이 나뉘지 않는 학과는 빈 목록일 수 있습니다)
    pub async fn majors(
        &mut self,
        year: u32,
        collage: &str,
        department: &str,
    ) -> Result<Vec<String>, RusaintError> {
        self.select_year(year).await?;
        self.select(Self::COLLAGE, collage).await?;
        self.select(Self::DEPARTMENT, department).await?;
        Ok(self.items(Self::MAJOR)?)
    }

    /// 입학년도·단과대학·학과(·전공)의 교과과정
    pub async fn curriculum(
        &mut self,
        year: u32,
        collage: &str,
        department: &str,
        major: Option<&str>,
    ) -> Result<Vec<CurriculumCourse>, RusaintError> {
        self.select_year(year).await?;
        self.select(Self::COLLAGE, collage).await?;
        self.select(Self::DEPARTMENT, department).await?;
        if let Some(major) = major.filter(|m| !m.trim().is_empty()) {
            self.select(Self::MAJOR, major).await?;
        }
        let press = ElementParser::new(self.client.body())
            .read(ButtonPressEventCommand::new(Self::SEARCH))?;
        self.client.process_event(false, press).await?;
        // 이 표(SapTableCS)는 처음에 20줄만 그리고, 나머지는 브라우저처럼 RequestData 이벤트로
        // 남은 줄 범위를 요청해야 받을 수 있습니다. 응답은 표 일부만 바꾸므로 받을 때마다 모읍니다.
        let mut courses: Vec<CurriculumCourse> = Vec::new();
        let total = ROW_COUNT
            .captures(self.client.body().raw_body())
            .and_then(|c| c.get(1)?.as_str().parse::<u32>().ok())
            .unwrap_or(0);
        loop {
            let before = courses.len();
            for course in parse_rows(self.client.body().raw_body()) {
                if !courses.iter().any(|c| c.same_row(&course)) {
                    courses.push(course);
                }
            }
            let loaded = courses.len() as u32;
            if loaded >= total || courses.len() == before {
                break;
            }
            let event = request_rows(Self::TABLE.id(), loaded, total - 1)?;
            self.client.process_event(true, event).await?;
        }
        if courses.is_empty() {
            return Err(ApplicationError::NoLectureResult.into());
        }
        Ok(courses)
    }

    /// 페이지를 새로 읽습니다.
    pub async fn reload(&mut self) -> Result<(), RusaintError> {
        self.client.reload().await?;
        Ok(())
    }
}
