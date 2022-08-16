use crate::http::ResponseWriter;
use crate::AppState;
use handlebars::{
    handlebars_helper, template::TemplateElement, Handlebars, Template, TemplateError,
};
use serde::ser::SerializeMap;
use serde::{Serialize, Serializer};
use sqlx::{Column, Database, Decode, Row};
use std::fs::DirEntry;
use serde_json::json;

pub struct RenderContext<'a> {
    app_state: &'a AppState,
    writer: ResponseWriter,
    current_component: Option<String>,
    error_depth: usize,
}

const DEFAULT_COMPONENT: &str = "default";
const MAX_ERROR_RECURSION: usize = 3;

impl RenderContext<'_> {
    pub fn new(app_state: &AppState, writer: ResponseWriter) -> RenderContext {
        RenderContext {
            app_state,
            writer,
            current_component: None,
            error_depth: 0,
        }
    }

    pub async fn handle_row(
        &mut self,
        row: sqlx::any::AnyRow,
    ) {
        let data = SerializeRow(row);
        log::debug!("Processing database row: {:?}", json!(data));
        let new_component = data.0.try_get::<&str, &str>("component");
        let current_component = &self.current_component;
        match (current_component, new_component) {
            (None, Ok("head")) | (None, Err(_)) => {
                self.render_template_with_data("shell_before", &&data);
                self.open_component_with_data(DEFAULT_COMPONENT.to_string(), &&data);
            }
            (None, new_component) => {
                self.render_template("shell_before");
                let component = new_component.unwrap_or(DEFAULT_COMPONENT).to_string();
                self.open_component_with_data(component, &&data);
            }
            (Some(current_component), Ok(new_component)) if new_component != current_component => {
                self.open_component_with_data(new_component.to_string(), &&data);
            }
            (Some(_), _) => {
                self.render_current_template_with_data(&&data);
            }
        }
    }

    pub async fn finish_query(
        &mut self,
        result: sqlx::any::AnyQueryResult,
    ) {
        log::trace!("finish_query: {:?}", result);
    }

    /// Handles the rendering of an error.
    /// Returns whether the error is irrecoverable and the rendering must stop
    pub fn handle_error(&mut self, error: &impl std::error::Error) -> std::io::Result<()> {
        self.error_depth += 1;
        if self.error_depth > MAX_ERROR_RECURSION {
            return Err(std::io::ErrorKind::Interrupted.into());
        }
        log::warn!("SQL error: {:?}", error);
        if self.current_component.is_some() {
            self.close_component();
        }
        self.open_component("error".to_string());
        let description = format!("{}", error);
        let mut backtrace = vec![];
        let mut source = error.source();
        while let Some(s) = source {
            backtrace.push(format!("{}", s));
            source = s.source()
        }
        self.render_current_template_with_data(&serde_json::json!({
            "description": description,
            "backtrace": backtrace
        }));
        self.close_component();
        self.error_depth -= 1;
        Ok(())
    }

    pub fn handle_result<R, E: std::error::Error>(&mut self, result: &Result<R, E>) -> std::io::Result<()> {
        if let Err(error) = result {
            self.handle_error(error)
        } else {
            Ok(())
        }
    }

    pub fn handle_result_and_log<R, E: std::error::Error>(&mut self, result: &Result<R, E>) {
        if let Err(e) = self.handle_result(result) {
            log::error!("{}", e);
        }
    }

    fn render_template(&mut self, name: &str) {
        self.render_template_with_data(name, &())
    }

    fn render_template_with_data<T: Serialize>(&mut self, name: &str, data: &T) {
        self.handle_result_and_log(&self.app_state.all_templates.handlebars.render_to_write(
            name,
            data,
            &self.writer,
        ));
    }

    fn render_current_template_with_data<T: Serialize>(&mut self, data: &T) {
        let name = self.current_component.as_ref().unwrap();
        self.handle_result_and_log(&self.app_state.all_templates.handlebars.render_to_write(
            name,
            data,
            &self.writer,
        ));
    }

    fn open_component(&mut self, component: String) {
        self.open_component_with_data(component, &json!(null));
    }

    fn open_component_with_data<T: Serialize>(&mut self, component: String, data: &T) {
        self.close_component();
        self.render_template_with_data(&[&component, "_before"].join(""), data);
        self.current_component = Some(component);
    }

    fn close_component(&mut self) {
        if let Some(component) = self.current_component.take() {
            self.render_template(&(component + "_after"));
            self.render_template("shell");
        }
    }
}

impl Drop for RenderContext<'_> {
    fn drop(&mut self) {
        if let Some(component) = self.current_component.take() {
            self.render_template(&(component + "_after"));
        }
        self.render_template("shell_after");
    }
}

struct SerializeRow<R: Row>(R);

impl<'r, R: Row> Serialize for &'r SerializeRow<R>
    where
        usize: sqlx::ColumnIndex<R>,
        &'r str: sqlx::Decode<'r, <R as Row>::Database>,
        f64: sqlx::Decode<'r, <R as Row>::Database>,
        i64: sqlx::Decode<'r, <R as Row>::Database>,
        bool: sqlx::Decode<'r, <R as Row>::Database>,
{
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
    {
        use sqlx::{TypeInfo, ValueRef};
        let columns = self.0.columns();
        let mut map = serializer.serialize_map(Some(columns.len()))?;
        for col in columns {
            let key = col.name();
            match self.0.try_get_raw(col.ordinal()) {
                Ok(raw_value) if !raw_value.is_null() => match raw_value.type_info().name() {
                    "REAL" | "FLOAT" | "NUMERIC" | "FLOAT4" | "FLOAT8" | "DOUBLE" => {
                        map_serialize::<_, _, f64>(&mut map, key, raw_value)
                    }
                    "INT" | "INTEGER" | "INT8" | "INT2" | "INT4" | "TINYINT" | "SMALLINT"
                    | "BIGINT" => map_serialize::<_, _, i64>(&mut map, key, raw_value),
                    "BOOL" | "BOOLEAN" => map_serialize::<_, _, bool>(&mut map, key, raw_value),
                    // Deserialize as a string by default
                    _ => map_serialize::<_, _, &str>(&mut map, key, raw_value),
                },
                _ => map.serialize_entry(key, &()), // Serialize null
            }?
        }
        map.end()
    }
}

fn map_serialize<'r, M: SerializeMap, DB: Database, T: Decode<'r, DB> + Serialize>(
    map: &mut M,
    key: &str,
    raw_value: <DB as sqlx::database::HasValueRef<'r>>::ValueRef,
) -> Result<(), M::Error> {
    let val = T::decode(raw_value).map_err(serde::ser::Error::custom)?;
    map.serialize_entry(key, &val)
}

struct SplitTemplate {
    before_list: Template,
    list_content: Template,
    after_list: Template,
}

fn split_template(mut original: Template) -> SplitTemplate {
    let mut elements_after = Vec::new();
    let mut mapping_after = Vec::new();
    let mut items_template = None;
    let found = original.elements.iter().position(is_template_list_item);
    if let Some(idx) = found {
        elements_after = original.elements.split_off(idx + 1);
        mapping_after = original.mapping.split_off(idx + 1);
        if let Some(TemplateElement::HelperBlock(tpl)) = original.elements.pop() {
            original.mapping.pop();
            items_template = tpl.template
        }
    }
    let mut list_content = items_template.unwrap_or_default();
    list_content.name = original.name.clone();
    SplitTemplate {
        before_list: Template {
            name: original.name.clone(),
            elements: original.elements,
            mapping: original.mapping,
        },
        list_content,
        after_list: Template {
            name: original.name,
            elements: elements_after,
            mapping: mapping_after,
        },
    }
}

fn is_template_list_item(element: &TemplateElement) -> bool {
    use handlebars::template::*;
    use Parameter::*;
    matches!(element,
                    TemplateElement::HelperBlock(tpl)
                        if matches!(&tpl.name, Name(name) if name == "each_row"))
}

pub struct AllTemplates {
    handlebars: Handlebars<'static>,
}

impl AllTemplates {
    pub fn init() -> Self {
        let mut handlebars = Handlebars::new();
        handlebars_helper!(stringify: |v: Json| v.to_string());
        handlebars.register_helper("stringify", Box::new(stringify));
        handlebars_helper!(default: |a: Json, b:Json| if dbg!(a).is_null() {b} else {a}.clone());
        handlebars.register_helper("default", Box::new(default));
        handlebars_helper!(entries: |v: Json | match v {
            serde_json::value::Value::Object(map) =>
                map.into_iter()
                    .map(|(k, v)| serde_json::json!({"key": k, "value": v}))
                    .collect(),
            serde_json::value::Value::Array(values) =>
                values.iter()
                    .enumerate()
                    .map(|(k, v)| serde_json::json!({"key": k, "value": v}))
                    .collect(),
            _ => vec![]
        });
        handlebars.register_helper("entries", Box::new(entries));
        let mut this = Self { handlebars };
        this.register_split("shell", include_str!("../templates/shell.handlebars"))
            .expect("Embedded shell template contains an error");
        this.register_split("error", include_str!("../templates/error.handlebars"))
            .expect("Embedded shell template contains an error");
        this.register_dir();
        this
    }

    fn register_split(&mut self, name: &str, tpl_str: &str) -> Result<(), TemplateError> {
        let mut tpl = Template::compile(tpl_str)?;
        tpl.name = Some(name.to_string());
        let split = split_template(tpl);
        self.handlebars
            .register_template(&[name, "before"].join("_"), split.before_list);
        self.handlebars.register_template(name, split.list_content);
        self.handlebars
            .register_template(&[name, "after"].join("_"), split.after_list);
        Ok(())
    }

    fn register_dir(&mut self) {
        let mut errors = vec![];
        match std::fs::read_dir("templates") {
            Ok(dir) => {
                for f in dir {
                    errors.extend(self.register_dir_entry(f).err());
                }
            }
            Err(e) => errors.push(Box::new(e)),
        }
        for err in errors {
            log::error!("Unable to register a template: {}", err);
        }
    }

    fn register_dir_entry(
        &mut self,
        entry: std::io::Result<DirEntry>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let path = entry?.path();
        if matches!(path.extension(), Some(x) if x == "handlebars") {
            let tpl_str = std::fs::read_to_string(&path)?;
            let name = path.file_stem().unwrap().to_string_lossy();
            self.register_split(&name, &tpl_str)?;
        }
        Ok(())
    }
}

#[test]
fn test_split_template() {
    let template = Template::compile(
        "Hello {{name}} ! \
        {{#each_row}}<li>{{this}}</li>{{/each_row}}\
        end",
    ).unwrap();
    let split = split_template(template);
    assert_eq!(split.before_list.elements, Template::compile("Hello {{name}} ! ").unwrap().elements);
    assert_eq!(split.list_content.elements, Template::compile("<li>{{this}}</li>").unwrap().elements);
    assert_eq!(split.after_list.elements, Template::compile("end").unwrap().elements);
}
