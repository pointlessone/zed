use std::any::{Any, TypeId};

use collections::HashMap;
use gpui::{
    actions, AnyElement, AnyView, AppContext, EventEmitter, FocusHandle, FocusableView,
    InteractiveElement, Model, Render, Task, View, WeakView,
};
use multi_buffer::MultiBuffer;
use project::{Project, ProjectEntryId, ProjectPath, WorktreeId};
use theme::ActiveTheme;
use ui::{
    div, h_flex, Color, Context, FluentBuilder, Icon, IconName, IntoElement, Label, LabelCommon,
    ParentElement, SharedString, Styled, ViewContext, VisualContext, WindowContext,
};
use workspace::{
    item::{BreadcrumbText, Item, ItemEvent, ItemHandle, TabContentParams},
    ItemNavHistory, Pane, ToolbarItemLocation, Workspace,
};

use crate::{Editor, EditorEvent};

actions!(project_diff, [Deploy]);

pub fn init(cx: &mut AppContext) {
    cx.observe_new_views(ProjectDiffEditor::register).detach();
}

struct ProjectDiffEditor {
    project_changes: HashMap<WorktreeId, HashMap<ProjectEntryId, Changes>>,
    project: Model<Project>,
    workspace: WeakView<Workspace>,
    focus_handle: FocusHandle,
    editor: View<Editor>,
    excerpts: Model<MultiBuffer>,
}

enum Changes {
    NotLoaded,
    Invalidated(Vec<()>),
    Present(Vec<()>),
}

impl ProjectDiffEditor {
    fn register(workspace: &mut Workspace, _: &mut ViewContext<Workspace>) {
        workspace.register_action(Self::deploy);
    }

    fn deploy(workspace: &mut Workspace, _: &Deploy, cx: &mut ViewContext<Workspace>) {
        if let Some(existing) = workspace.item_of_type::<Self>(cx) {
            workspace.activate_item(&existing, cx);
        } else {
            let workspace_handle = cx.view().downgrade();
            let project_diff =
                cx.new_view(|cx| Self::new(workspace.project().clone(), workspace_handle, cx));
            workspace.add_item_to_active_pane(Box::new(project_diff), None, cx);
        }
    }

    fn new(
        project: Model<Project>,
        workspace: WeakView<Workspace>,
        cx: &mut ViewContext<Self>,
    ) -> Self {
        // TODO kb diff change subscriptions + edited subscriptions
        // for that, needed:
        // * `-20/+50` stats retrieval: some background process that reacts on file changes
        // * workspace change tracking: would project events be enough? WorktreeAdded/Removed/UpdatedEntries/UpdatedGitRepositories?
        let focus_handle = cx.focus_handle();

        let excerpts = cx.new_model(|cx| {
            let project = project.read(cx);
            MultiBuffer::new(project.replica_id(), project.capability())
        });

        let editor = cx.new_view(|cx| {
            Editor::for_multibuffer(excerpts.clone(), Some(project.clone()), true, cx)
        });

        Self {
            project,
            workspace,
            project_changes: HashMap::default(),
            focus_handle,
            editor,
            excerpts,
        }
    }
}

impl EventEmitter<EditorEvent> for ProjectDiffEditor {}

impl FocusableView for ProjectDiffEditor {
    fn focus_handle(&self, _: &AppContext) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for ProjectDiffEditor {
    type Event = EditorEvent;

    fn to_item_events(event: &EditorEvent, f: impl FnMut(ItemEvent)) {
        Editor::to_item_events(event, f)
    }

    fn deactivated(&mut self, cx: &mut ViewContext<Self>) {
        self.editor.update(cx, |editor, cx| editor.deactivated(cx));
    }

    fn navigate(&mut self, data: Box<dyn Any>, cx: &mut ViewContext<Self>) -> bool {
        self.editor
            .update(cx, |editor, cx| editor.navigate(data, cx))
    }

    fn tab_tooltip_text(&self, _: &AppContext) -> Option<SharedString> {
        Some("Project Diff".into())
    }

    fn tab_content(&self, params: TabContentParams, _: &WindowContext) -> AnyElement {
        if self.project_changes.is_empty() {
            Label::new("No changes")
                .color(if params.selected {
                    Color::Default
                } else {
                    Color::Muted
                })
                .into_any_element()
        } else {
            h_flex()
                .gap_1()
                .when(true, |then| {
                    then.child(
                        h_flex()
                            .gap_1()
                            .child(Icon::new(IconName::XCircle).color(Color::Error))
                            .child(Label::new(self.project_changes.len().to_string()).color(
                                if params.selected {
                                    Color::Default
                                } else {
                                    Color::Muted
                                },
                            )),
                    )
                })
                .when(true, |then| {
                    then.child(
                        h_flex()
                            .gap_1()
                            .child(Icon::new(IconName::ExclamationTriangle).color(Color::Warning))
                            .child(Label::new(self.project_changes.len().to_string()).color(
                                if params.selected {
                                    Color::Default
                                } else {
                                    Color::Muted
                                },
                            )),
                    )
                })
                .into_any_element()
        }
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        Some("project diagnostics")
    }

    fn for_each_project_item(
        &self,
        cx: &AppContext,
        f: &mut dyn FnMut(gpui::EntityId, &dyn project::Item),
    ) {
        self.editor.for_each_project_item(cx, f)
    }

    fn is_singleton(&self, _: &AppContext) -> bool {
        false
    }

    fn set_nav_history(&mut self, nav_history: ItemNavHistory, cx: &mut ViewContext<Self>) {
        self.editor.update(cx, |editor, _| {
            editor.set_nav_history(Some(nav_history));
        });
    }

    fn clone_on_split(
        &self,
        _workspace_id: Option<workspace::WorkspaceId>,
        cx: &mut ViewContext<Self>,
    ) -> Option<View<Self>>
    where
        Self: Sized,
    {
        Some(cx.new_view(|cx| {
            ProjectDiffEditor::new(self.project.clone(), self.workspace.clone(), cx)
        }))
    }

    fn is_dirty(&self, cx: &AppContext) -> bool {
        self.excerpts.read(cx).is_dirty(cx)
    }

    fn has_conflict(&self, cx: &AppContext) -> bool {
        self.excerpts.read(cx).has_conflict(cx)
    }

    fn can_save(&self, _: &AppContext) -> bool {
        true
    }

    fn save(
        &mut self,
        format: bool,
        project: Model<Project>,
        cx: &mut ViewContext<Self>,
    ) -> Task<anyhow::Result<()>> {
        self.editor.save(format, project, cx)
    }

    fn save_as(
        &mut self,
        _: Model<Project>,
        _: ProjectPath,
        _: &mut ViewContext<Self>,
    ) -> Task<anyhow::Result<()>> {
        unreachable!()
    }

    fn reload(
        &mut self,
        project: Model<Project>,
        cx: &mut ViewContext<Self>,
    ) -> Task<anyhow::Result<()>> {
        self.editor.reload(project, cx)
    }

    fn act_as_type<'a>(
        &'a self,
        type_id: TypeId,
        self_handle: &'a View<Self>,
        _: &'a AppContext,
    ) -> Option<AnyView> {
        if type_id == TypeId::of::<Self>() {
            Some(self_handle.to_any())
        } else if type_id == TypeId::of::<Editor>() {
            Some(self.editor.to_any())
        } else {
            None
        }
    }

    fn breadcrumb_location(&self) -> ToolbarItemLocation {
        ToolbarItemLocation::PrimaryLeft
    }

    fn breadcrumbs(&self, theme: &theme::Theme, cx: &AppContext) -> Option<Vec<BreadcrumbText>> {
        self.editor.breadcrumbs(theme, cx)
    }

    fn added_to_workspace(&mut self, workspace: &mut Workspace, cx: &mut ViewContext<Self>) {
        self.editor
            .update(cx, |editor, cx| editor.added_to_workspace(workspace, cx));
    }

    fn serialized_item_kind() -> Option<&'static str> {
        Some("project_diff")
    }

    fn deserialize(
        project: Model<Project>,
        workspace: WeakView<Workspace>,
        _workspace_id: workspace::WorkspaceId,
        _item_id: workspace::ItemId,
        cx: &mut ViewContext<Pane>,
    ) -> Task<anyhow::Result<View<Self>>> {
        Task::ready(Ok(cx.new_view(|cx| Self::new(project, workspace, cx))))
    }
}

impl Render for ProjectDiffEditor {
    fn render(&mut self, cx: &mut ViewContext<Self>) -> impl IntoElement {
        let child = if self.project_changes.is_empty() {
            div()
                .bg(cx.theme().colors().editor_background)
                .flex()
                .items_center()
                .justify_center()
                .size_full()
                .child(Label::new("No changes in the workspace"))
        } else {
            div().size_full().child(self.editor.clone())
        };

        div()
            .track_focus(&self.focus_handle)
            .size_full()
            .child(child)
    }
}
