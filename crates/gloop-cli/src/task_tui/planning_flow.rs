//! The guided plan editor: named actions, explicit model choices, then execution.

use std::{fmt::Write as _, path::Path};

use anyhow::{Context, Result, ensure};

use super::{App, Screen, blank_request, string};
use crate::{
    atomic_write,
    jobs::TaskKind,
    planning::{DraftPlan, MAX_STEPS, PlanStep},
    templates,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Page {
    Overview,
    Step,
    Title,
    Instructions,
    Completion,
    Profile,
    Model,
    CustomModel,
    Dependencies,
    Save,
}

impl Page {
    pub(super) fn is_input(self) -> bool {
        matches!(
            self,
            Self::Title | Self::Instructions | Self::Completion | Self::CustomModel | Self::Save
        )
    }
    pub(super) fn multiline(self) -> bool {
        matches!(self, Self::Instructions | Self::Completion)
    }
}

impl App {
    pub(super) fn start_planning(&mut self) {
        self.start_draft(None);
        self.draft.kind = TaskKind::Planning;
        self.draft.timeout_seconds = 300;
        self.plan = None;
    }

    pub(super) fn open_plan(&mut self) -> Result<()> {
        self.plan = Some(DraftPlan::from_task(&self.task)?);
        self.message = self.text("Plan created. Check the steps and their AI tools. No implementation has started.", "手順案を作成しました。内容と担当AIを確認してください。実作業はまだ開始していません。").to_owned();
        self.go(Screen::Plan(Page::Overview));
        Ok(())
    }

    fn plan_model_choices(&self) -> Vec<String> {
        let plan = self.plan.as_ref().expect("plan screen has a draft");
        self.catalog_models(Some(&plan.bindings[plan.selected_step].profile))
    }

    pub(super) fn plan_choices(&self, page: Page) -> Vec<String> {
        let plan = self.plan.as_ref().expect("plan screen has a draft");
        let step = &plan.plan.steps[plan.selected_step];
        let t = |en, ja| self.text(en, ja).to_owned();
        match page {
            Page::Overview => {
                let mut choices: Vec<_> = plan
                    .plan
                    .steps
                    .iter()
                    .zip(&plan.bindings)
                    .enumerate()
                    .map(|(index, (step, binding))| {
                        format!(
                            "{}. {}  · {} / {}",
                            index + 1,
                            step.title,
                            binding.profile,
                            binding.model.as_deref().unwrap_or("default")
                        )
                    })
                    .collect();
                choices.push(format!(
                    "▶ {}  ({} {})",
                    self.text("Run these steps", "この手順で実行"),
                    plan.plan.steps.len(),
                    self.text("AI calls maximum", "回まで")
                ));
                choices.push(t("+ Add a step", "+ 手順を追加"));
                choices.push(t("Save this workflow", "この手順を保存"));
                choices.push(t(
                    "Edit the request and create a new plan",
                    "依頼文に戻って作り直す",
                ));
                choices
            }
            Page::Step => vec![
                t("Edit title", "手順の名前を変更"),
                t("Edit instructions", "作業内容を変更"),
                t("Edit completion criteria", "完了条件を変更"),
                t("Choose AI tool", "担当AIを変更"),
                t("Choose model", "モデルを変更"),
                t("Choose prerequisite steps", "先に必要な手順を変更"),
                t("Remove this step", "この手順を削除"),
                t("Back to all steps", "手順一覧に戻る"),
            ],
            Page::Profile => self
                .available()
                .iter()
                .map(|profile| string(profile, "name").to_owned())
                .collect(),
            Page::Model => {
                let mut choices = vec![t("Use this tool's default", "このツールの既定モデル")];
                choices.extend(self.plan_model_choices());
                choices.push(t("Enter a model id / alias…", "モデルID・別名を直接入力…"));
                choices
            }
            Page::Dependencies => {
                let mut choices: Vec<_> = plan
                    .plan
                    .steps
                    .iter()
                    .filter(|candidate| candidate.id != step.id)
                    .map(|candidate| {
                        format!(
                            "[{}] {}",
                            if step.depends_on.contains(&candidate.id) {
                                "✓"
                            } else {
                                " "
                            },
                            candidate.title
                        )
                    })
                    .collect();
                choices.push(t("Done", "選択を確定"));
                choices
            }
            _ => vec![],
        }
    }

    pub(super) fn plan_heading(&self, page: Page) -> &str {
        match page {
            Page::Overview => self.text("Review your work plan", "手順案を確認・調整"),
            Page::Step => self.text("Edit this step", "この手順を編集"),
            Page::Title => self.text("Step title", "手順の名前"),
            Page::Instructions => self.text("What should this step do?", "この手順で何をする？"),
            Page::Completion => {
                self.text("How will you know it is done?", "何を確認できれば完了？")
            }
            Page::Profile | Page::Model | Page::CustomModel => {
                self.text("Choose who performs this step", "この手順の担当を選ぶ")
            }
            Page::Dependencies => self.text(
                "Which steps must finish first?",
                "先に完了している必要がある手順は？",
            ),
            Page::Save => self.text("Name this saved workflow", "保存する手順に名前を付ける"),
        }
    }

    pub(super) fn plan_description(&self, page: Page) -> String {
        let plan = self.plan.as_ref().expect("plan screen has a draft");
        let step = &plan.plan.steps[plan.selected_step];
        let binding = &plan.bindings[plan.selected_step];
        match page {
            Page::Overview => format!("{}\n{}\n\n{}", self.text("Enter a step to read or edit it. Choose ‘Run these steps’ after checking the plan.", "手順を選んで Enter で内容・担当を編集。確認後に「この手順で実行」を選びます。"), self.text("Steps run one at a time. Prerequisite results are passed to dependent steps.", "手順は一つずつ実行し、前提になっている手順の結果を引き継ぎます。"), plan.goal),
            Page::Step => {
                let dependencies = step.depends_on.iter().filter_map(|id| plan.plan.steps.iter().find(|other| &other.id == id).map(|other| other.title.as_str())).collect::<Vec<_>>().join(" / ");
                format!("{}\n{} / {}\n\n{}\n\n{}: {}\n{}: {}\n{}: {}", step.title, binding.profile, binding.model.as_deref().unwrap_or("default"), step.instructions, self.text("Done when", "完了条件"), step.completion_criteria, self.text("Needs", "前提手順"), if dependencies.is_empty() { "—" } else { &dependencies }, self.text("Planned files", "対象ファイル"), step.owned_files.join(", "))
            }
            Page::Instructions => self.text("Write the concrete work for this step. Alt-Enter adds a line; Enter saves.", "この手順で行う作業を書きます。Alt-Enter で改行、Enter で反映。").to_owned(),
            Page::Completion => self.text("State what must be verified: expected behavior, test results, or a deliverable.", "期待する動作・テスト結果・成果物など、確認できる完了条件を書きます。").to_owned(),
            Page::Profile | Page::Model | Page::CustomModel => format!("{}\n{}", step.title, self.text("This changes only the selected step. No AI call is made here.", "選択中の手順の担当だけを変更します。この操作ではAIを呼びません。")),
            Page::Dependencies => self.text("Enter toggles a prerequisite. Checked steps must finish first and their results are attached. Cycles are rejected.", "Enter で前提手順を切り替えます。選んだ手順の完了を待ち、その結果を渡します。循環する指定はできません。").to_owned(),
            Page::Save => self.text("Use lowercase letters, numbers, and hyphens. The workflow will appear under Saved workflows; an existing file is never overwritten.", "英小文字・数字・ハイフンで名前を入力。保存したワークフローから呼び出せます。既存ファイルには上書きしません。").to_owned(),
            Page::Title => self.text("Use a short name that explains the outcome.", "何ができるようになるかが分かる短い名前にします。").to_owned(),
        }
    }

    fn plan_input(&mut self, page: Page, text: String) {
        self.input = text;
        self.cursor = self.input.len();
        self.go(Screen::Plan(page));
    }

    pub(super) fn back_plan(&mut self, page: Page) {
        self.go(Screen::Plan(match page {
            Page::Overview => {
                self.go(Screen::Home);
                return;
            }
            Page::Step | Page::Save => Page::Overview,
            Page::CustomModel => Page::Model,
            _ => Page::Step,
        }));
    }

    pub(super) fn enter_plan(&mut self, page: Page) -> Result<()> {
        match page {
            Page::Overview => self.enter_plan_overview(),
            Page::Step => self.enter_plan_step(),
            Page::Title | Page::Instructions | Page::Completion => {
                ensure!(
                    !self.input.trim().is_empty(),
                    "{}",
                    self.text(
                        "Enter a value before saving.",
                        "空欄にはできません。内容を入力してください。"
                    )
                );
                let plan = self.plan.as_mut().context("no plan is open")?;
                let step = &mut plan.plan.steps[plan.selected_step];
                match page {
                    Page::Title => self.input.trim().clone_into(&mut step.title),
                    Page::Instructions => self.input.trim().clone_into(&mut step.instructions),
                    Page::Completion => self.input.trim().clone_into(&mut step.completion_criteria),
                    _ => unreachable!(),
                }
                self.go(Screen::Plan(Page::Step));
                Ok(())
            }
            Page::Profile => {
                let Some(profile) = self
                    .available()
                    .get(self.selected)
                    .map(|p| string(p, "name").to_owned())
                else {
                    return Ok(());
                };
                let plan = self.plan.as_mut().context("no plan is open")?;
                plan.bindings[plan.selected_step].profile = profile;
                plan.bindings[plan.selected_step].model = None;
                self.go(Screen::Plan(Page::Model));
                Ok(())
            }
            Page::Model => {
                let models = self.plan_model_choices();
                if self.selected == models.len() + 1 {
                    self.plan_input(Page::CustomModel, String::new());
                    return Ok(());
                }
                let model = self
                    .selected
                    .checked_sub(1)
                    .and_then(|index| models.get(index).cloned());
                let plan = self.plan.as_mut().context("no plan is open")?;
                plan.bindings[plan.selected_step].model = model;
                self.go(Screen::Plan(Page::Step));
                Ok(())
            }
            Page::CustomModel => {
                ensure!(!self.input.trim().is_empty(), "enter a model id");
                let plan = self.plan.as_mut().context("no plan is open")?;
                plan.bindings[plan.selected_step].model = Some(self.input.trim().to_owned());
                self.go(Screen::Plan(Page::Step));
                Ok(())
            }
            Page::Dependencies => self.toggle_plan_dependency(),
            Page::Save => self.save_plan(),
        }
    }

    fn enter_plan_overview(&mut self) -> Result<()> {
        let plan = self.plan.as_ref().context("no plan is open")?;
        let count = plan.plan.steps.len();
        if self.selected < count {
            self.plan.as_mut().expect("plan exists").selected_step = self.selected;
            self.go(Screen::Plan(Page::Step));
            return Ok(());
        }
        match self.selected - count {
            0 => {
                let graph = plan.graph()?;
                self.draft = blank_request();
                self.draft.goal.clone_from(&plan.goal);
                self.draft.max_calls = u32::try_from(count)?;
                self.draft.graph = Some(graph);
                self.submission = None;
                self.go(Screen::Confirm);
            }
            1 => self.add_plan_step()?,
            2 => self.plan_input(
                Page::Save,
                format!(
                    "workflow-{}",
                    &plan.source_job[..plan.source_job.len().min(12)]
                ),
            ),
            3 => {
                let goal = plan.goal.clone();
                self.start_planning();
                self.input = goal;
                self.cursor = self.input.len();
            }
            _ => {}
        }
        Ok(())
    }

    fn enter_plan_step(&mut self) -> Result<()> {
        let plan = self.plan.as_ref().context("no plan is open")?;
        let step = &plan.plan.steps[plan.selected_step];
        match self.selected {
            0 => self.plan_input(Page::Title, step.title.clone()),
            1 => self.plan_input(Page::Instructions, step.instructions.clone()),
            2 => self.plan_input(Page::Completion, step.completion_criteria.clone()),
            3 => self.go(Screen::Plan(Page::Profile)),
            4 => self.go(Screen::Plan(Page::Model)),
            5 => self.go(Screen::Plan(Page::Dependencies)),
            6 => {
                self.plan.as_mut().expect("plan exists").remove_selected()?;
                self.go(Screen::Plan(Page::Overview));
            }
            _ => self.go(Screen::Plan(Page::Overview)),
        }
        Ok(())
    }

    fn add_plan_step(&mut self) -> Result<()> {
        ensure!(self.plan.as_ref().context("no plan is open")?.plan.steps.len() < MAX_STEPS, "{}", self.text("A plan supports up to eight steps. Split a larger request into separate workflows.", "手順は8つまでです。大きな依頼は複数のワークフローに分けてください。"));
        let title = self.text("New step", "新しい作業").to_owned();
        let plan = self.plan.as_mut().expect("plan exists");
        let id = (1..=MAX_STEPS + 1)
            .map(|index| format!("step_{index}"))
            .find(|id| plan.plan.steps.iter().all(|step| &step.id != id))
            .expect("a free step id exists");
        let depends_on = plan
            .plan
            .steps
            .last()
            .map(|step| vec![step.id.clone()])
            .unwrap_or_default();
        plan.plan.steps.push(PlanStep {
            id,
            title,
            instructions: String::new(),
            completion_criteria: String::new(),
            owned_files: vec![],
            depends_on,
        });
        plan.bindings
            .push(plan.bindings[plan.selected_step].clone());
        plan.selected_step = plan.plan.steps.len() - 1;
        self.go(Screen::Plan(Page::Step));
        self.message = self
            .text(
                "Enter the instructions and completion criteria for this new step.",
                "追加した手順の作業内容と完了条件を入力してください。",
            )
            .to_owned();
        Ok(())
    }

    fn toggle_plan_dependency(&mut self) -> Result<()> {
        let plan = self.plan.as_mut().context("no plan is open")?;
        let id = plan.plan.steps[plan.selected_step].id.clone();
        let candidates: Vec<_> = plan
            .plan
            .steps
            .iter()
            .filter(|step| step.id != id)
            .map(|step| step.id.clone())
            .collect();
        if self.selected >= candidates.len() {
            self.go(Screen::Plan(Page::Step));
            return Ok(());
        }
        let candidate = &candidates[self.selected];
        let old = plan.plan.steps[plan.selected_step].depends_on.clone();
        if old.contains(candidate) {
            plan.plan.steps[plan.selected_step]
                .depends_on
                .retain(|id| id != candidate);
        } else {
            plan.plan.steps[plan.selected_step]
                .depends_on
                .push(candidate.clone());
        }
        if let Err(error) = plan.plan.validate_dependencies() {
            plan.plan.steps[plan.selected_step].depends_on = old;
            return Err(error);
        }
        Ok(())
    }

    fn save_plan(&mut self) -> Result<()> {
        let name = self.input.trim();
        templates::validate_template_lookup_name(name).map_err(anyhow::Error::msg)?;
        let mut graph = self.plan.as_ref().context("no plan is open")?.graph()?;
        name.clone_into(&mut graph.metadata.name);
        templates::ensure_managed_directory(&self.repo, Path::new(templates::GRAPHS_DIR))?;
        std::fs::create_dir_all(templates::graphs_dir(&self.repo))?;
        let path = templates::graph_path(&self.repo, name);
        atomic_write::write_text_no_replace_sync(&path, &graph.to_yaml()?)?;
        let saved = gloop_core::Graph::from_path(&path)?;
        ensure!(
            saved == graph,
            "saved workflow does not match the reviewed plan"
        );
        self.message.clear();
        let _ = write!(
            self.message,
            "{}: {}",
            if self.lang == crate::i18n::Language::Ja {
                "保存しました"
            } else {
                "Saved"
            },
            path.display()
        );
        self.go(Screen::Plan(Page::Overview));
        Ok(())
    }
}
