use crate::templates::SplitTemplate;
use crate::AppState;
use anyhow::Context as AnyhowContext;
use handlebars::{BlockContext, Context, Handlebars, JsonValue, RenderError, Renderable};
use serde::Serialize;
use serde_json::{json, Value};
use std::borrow::Cow;

pub struct RenderContext<'a, W: std::io::Write> {
    app_state: &'a AppState,
    pub writer: W,
    current_component: Option<SplitTemplateRenderer<'a>>,
    shell_renderer: SplitTemplateRenderer<'a>,
    recursion_depth: usize,
    current_statement: usize,
}

const DEFAULT_COMPONENT: &str = "default";
const MAX_RECURSION_DEPTH: usize = 256;

impl<W: std::io::Write> RenderContext<'_, W> {
    pub fn new(app_state: &AppState, writer: W) -> RenderContext<W> {
        let shell_renderer =
            Self::create_renderer("shell", app_state).expect("shell must always exist");
        RenderContext {
            app_state,
            writer,
            current_component: None,
            shell_renderer,
            recursion_depth: 0,
            current_statement: 1,
        }
    }

    pub fn handle_row(&mut self, data: &JsonValue) -> anyhow::Result<()> {
        log::debug!(
            "<- Processing database row: {}",
            serde_json::to_string(&data).unwrap_or_else(|e| e.to_string())
        );
        let new_component = data
            .as_object()
            .and_then(|o| o.get("component"))
            .and_then(|c| c.as_str());
        let current_component = self.current_component.as_ref().map(|c| c.name());
        match (current_component, new_component) {
            (None, Some("head")) | (None, None) => {
                self.shell_renderer
                    .render_start(&mut self.writer, json!(&data))?;
                self.open_component_with_data(DEFAULT_COMPONENT, &data)?;
            }
            (None, new_component) => {
                self.shell_renderer
                    .render_start(&mut self.writer, json!(null))?;
                let component = new_component.unwrap_or(DEFAULT_COMPONENT);
                self.open_component_with_data(component, &data)?;
            }
            (Some(_current_component), Some("dynamic")) => {
                self.render_dynamic(data)?;
            }
            (Some(_current_component), Some(new_component)) => {
                self.open_component_with_data(new_component, &data)?;
            }
            (Some(_), _) => {
                self.render_current_template_with_data(&data)?;
            }
        }
        Ok(())
    }

    fn render_dynamic(&mut self, data: &Value) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.recursion_depth <= MAX_RECURSION_DEPTH,
            "Maximum recursion depth exceeded in the dynamic component."
        );
        let properties: Vec<Cow<JsonValue>> = data
            .get("properties")
            .and_then(|props| match props {
                Value::String(s) => match serde_json::from_str::<JsonValue>(s).ok()? {
                    Value::Array(values) => Some(values.into_iter().map(Cow::Owned).collect()),
                    obj @ Value::Object(_) => Some(vec![Cow::Owned(obj)]),
                    _ => None,
                },
                obj @ Value::Object(_) => Some(vec![Cow::Borrowed(obj)]),
                _ => None,
            })
            .context(
                "The dynamic component requires a parameter called 'parameters' that is a json ",
            )?;
        for p in properties {
            self.recursion_depth += 1;
            let res = self.handle_row(&p);
            self.recursion_depth -= 1;
            res?;
        }
        Ok(())
    }

    pub async fn finish_query(&mut self) -> anyhow::Result<()> {
        log::debug!("-> Query {} finished", self.current_statement);
        self.current_statement += 1;
        Ok(())
    }

    /// Handles the rendering of an error.
    /// Returns whether the error is irrecoverable and the rendering must stop
    pub fn handle_error(&mut self, error: &impl std::error::Error) -> anyhow::Result<()> {
        log::warn!("SQL error: {:?}", error);
        if self.current_component.is_some() {
            self.close_component()?;
        } else {
            self.shell_renderer
                .render_start(&mut self.writer, json!(null))?;
        }
        let saved_component = self.current_component.take();
        self.open_component("error")?;
        let description = format!("{}", error);
        let mut backtrace = vec![];
        let mut source = error.source();
        while let Some(s) = source {
            backtrace.push(format!("{}", s));
            source = s.source()
        }
        self.render_current_template_with_data(&json!({
            "query_number": self.current_statement,
            "description": description,
            "backtrace": backtrace
        }))?;
        self.close_component()?;
        self.current_component = saved_component;
        Ok(())
    }

    pub fn handle_anyhow_error(&mut self, error: &anyhow::Error) -> anyhow::Result<()> {
        let std_err = AsRef::<(dyn std::error::Error + 'static)>::as_ref(error);
        self.handle_error(&std_err)
    }

    pub fn handle_result<R, E: std::error::Error>(
        &mut self,
        result: &Result<R, E>,
    ) -> anyhow::Result<()> {
        if let Err(error) = result {
            self.handle_error(&error)
        } else {
            Ok(())
        }
    }

    pub fn handle_result_and_log<R, E: std::error::Error>(&mut self, result: &Result<R, E>) {
        if let Err(e) = self.handle_result(result) {
            log::error!("{}", e);
        }
    }

    fn render_current_template_with_data<T: Serialize>(&mut self, data: &T) -> anyhow::Result<()> {
        use anyhow::Context;
        let rdr = self.current_component.as_mut().with_context(|| {
            format!(
                "Tried to render the following data but no component is selected: {}",
                serde_json::to_string(data).unwrap_or_default()
            )
        })?;
        rdr.render_item(&mut self.writer, json!(data))?;
        self.shell_renderer
            .render_item(&mut self.writer, JsonValue::Null)?;
        Ok(())
    }

    fn open_component(&mut self, component: &str) -> anyhow::Result<()> {
        self.open_component_with_data(component, &json!(null))
    }

    fn create_renderer<'a>(
        component: &str,
        app_state: &'a AppState,
    ) -> anyhow::Result<SplitTemplateRenderer<'a>> {
        use anyhow::Context;
        let split_template = app_state
            .all_templates
            .split_templates
            .get(component)
            .with_context(|| format!("The component '{component}' was not found."))?;
        Ok(SplitTemplateRenderer::new(
            split_template,
            &app_state.all_templates.handlebars,
        ))
    }

    fn set_current_component(&mut self, component: &str) -> anyhow::Result<()> {
        self.current_component = Some(Self::create_renderer(component, self.app_state)?);
        Ok(())
    }

    fn open_component_with_data<T: Serialize>(
        &mut self,
        component: &str,
        data: &T,
    ) -> anyhow::Result<()> {
        self.close_component()?;
        self.set_current_component(component)?;
        self.current_component
            .as_mut()
            .unwrap()
            .render_start(&mut self.writer, json!(data))?;
        Ok(())
    }

    fn close_component(&mut self) -> anyhow::Result<()> {
        if let Some(component) = &mut self.current_component {
            component.render_end(&mut self.writer)?;
        }
        Ok(())
    }

    pub fn close(mut self) -> W {
        if let Some(mut component) = self.current_component.take() {
            let res = component.render_end(&mut self.writer);
            self.handle_result_and_log(&res);
        }
        let res = self.shell_renderer.render_end(&mut self.writer);
        self.handle_result_and_log(&res);
        self.writer
    }
}

struct HandlebarWriterOutput<W: std::io::Write>(W);

impl<W: std::io::Write> handlebars::Output for HandlebarWriterOutput<W> {
    fn write(&mut self, seg: &str) -> std::io::Result<()> {
        std::io::Write::write_all(&mut self.0, seg.as_bytes())
    }
}

pub struct SplitTemplateRenderer<'registry> {
    split_template: &'registry SplitTemplate,
    block_context: Option<BlockContext<'registry>>,
    registry: &'registry Handlebars<'registry>,
    row_index: usize,
}

impl<'reg> SplitTemplateRenderer<'reg> {
    fn new(split_template: &'reg SplitTemplate, registry: &'reg Handlebars<'reg>) -> Self {
        Self {
            split_template,
            block_context: None,
            registry,
            row_index: 0,
        }
    }
    fn name(&self) -> &str {
        self.split_template
            .list_content
            .name
            .as_deref()
            .unwrap_or_default()
    }

    fn render_start<W: std::io::Write>(
        &mut self,
        writer: W,
        data: JsonValue,
    ) -> Result<(), handlebars::RenderError> {
        let mut render_context = handlebars::RenderContext::new(None);
        let mut ctx = Context::from(data);
        let mut output = HandlebarWriterOutput(writer);
        self.split_template.before_list.render(
            self.registry,
            &ctx,
            &mut render_context,
            &mut output,
        )?;
        let mut blk = render_context
            .block_mut()
            .map(std::mem::take)
            .unwrap_or_default();
        blk.set_base_value(std::mem::take(ctx.data_mut()));
        self.block_context = Some(blk);
        self.row_index = 0;
        Ok(())
    }

    fn render_item<W: std::io::Write>(
        &mut self,
        writer: W,
        data: JsonValue,
    ) -> Result<(), RenderError> {
        if let Some(block_context) = self.block_context.take() {
            let mut render_context = handlebars::RenderContext::new(None);
            render_context.push_block(block_context);
            let mut blk = BlockContext::new();
            blk.set_base_value(data);
            blk.set_local_var("row_index", JsonValue::Number(self.row_index.into()));
            render_context.push_block(blk);
            let ctx = Context::null();
            let mut output = HandlebarWriterOutput(writer);
            self.split_template.list_content.render(
                self.registry,
                &ctx,
                &mut render_context,
                &mut output,
            )?;
            render_context.pop_block();
            self.block_context = render_context.block_mut().map(std::mem::take);
            self.row_index += 1;
        }
        Ok(())
    }

    fn render_end<W: std::io::Write>(&mut self, writer: W) -> Result<(), RenderError> {
        if let Some(block_context) = self.block_context.take() {
            let mut render_context = handlebars::RenderContext::new(None);
            render_context.push_block(block_context);
            let ctx = Context::null();
            let mut output = HandlebarWriterOutput(writer);
            self.split_template.after_list.render(
                self.registry,
                &ctx,
                &mut render_context,
                &mut output,
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::templates::split_template;
    use handlebars::Template;

    #[test]
    fn test_split_template_render() -> anyhow::Result<()> {
        let reg = Handlebars::new();
        let template = Template::compile(
            "Hello {{name}} !\
        {{#each_row}} ({{x}} : {{../name}}) {{/each_row}}\
        Goodbye {{name}}",
        )?;
        let split = split_template(template);
        let mut output = Vec::new();
        let mut rdr = SplitTemplateRenderer::new(&split, &reg);
        rdr.render_start(&mut output, json!({"name": "SQL"}))?;
        rdr.render_item(&mut output, json!({"x": 1}))?;
        rdr.render_item(&mut output, json!({"x": 2}))?;
        rdr.render_end(&mut output)?;
        assert_eq!(output, b"Hello SQL ! (1 : SQL)  (2 : SQL) Goodbye SQL");
        Ok(())
    }
}
