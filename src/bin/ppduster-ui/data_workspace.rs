//! Shared native workspace for data pipelines; the same graph format as Peregon.
use super::*;
use ppduster::data_graph::{DataDocument, DataNode, DataPosition};
use serde_json::{json, Value};

pub(super) struct DataWorkspace {
    pub document: DataDocument,
    pub dirty: bool,
    pub path: Option<PathBuf>,
    selected: Option<String>,
    result: Value,
    result_revision: Option<u64>,
    receiver: Option<Receiver<(u64, Value)>>,
    scene: Rect,
    connecting: Option<String>,
    search: String,
    message: Option<String>,
    tab: usize,
    config_text: String,
    arrays: BTreeMap<String, (String, Vec<Value>)>,
    pending: Option<(DataDocument, Option<PathBuf>)>,
    pub close_pending: bool,
}
impl Default for DataWorkspace {
    fn default() -> Self {
        Self {
            document: DataDocument::default(),
            dirty: false,
            path: None,
            selected: Some("source".into()),
            result: Value::Null,
            result_revision: None,
            receiver: None,
            scene: Rect::from_min_size(Pos2::ZERO, Vec2::new(880., 460.)),
            connecting: None,
            search: String::new(),
            message: None,
            tab: 0,
            config_text: String::new(),
            arrays: BTreeMap::new(),
            pending: None,
            close_pending: false,
        }
    }
}
const BLOCKS: &[(&str, &str, &str)] = &[
    ("source.json", "JSON", "Источник"),
    ("source.csv", "CSV", "Источник"),
    ("source.list", "Список", "Источник"),
    ("transform.filter", "Фильтр", "Преобразование"),
    ("transform.project", "Выбрать поля", "Преобразование"),
    ("transform.template", "Шаблон значения", "Преобразование"),
    ("sink.json", "JSON", "Результат"),
    ("sink.csv", "CSV", "Результат"),
    ("sink.xml", "XML", "Результат"),
    ("sink.sql", "SQL", "Результат"),
    ("sink.flat", "Плоский список", "Результат"),
    ("sink.template", "Текст по шаблону", "Результат"),
    ("sink.join", "Объединить значения", "Результат"),
];
impl DataWorkspace {
    fn changed(&mut self) {
        self.dirty = true;
        self.document.graph.revision += 1;
    }
    fn select(&mut self, id: String) {
        self.selected = Some(id);
        self.config_text.clear();
    }
    fn apply(&mut self, doc: DataDocument, path: Option<PathBuf>) {
        self.selected = doc.graph.nodes.first().map(|node| node.id.clone());
        self.document = doc;
        self.path = path;
        self.dirty = false;
        self.receiver = None;
        self.result = Value::Null;
        self.result_revision = None;
        self.config_text.clear();
        self.connecting = None;
        self.message = None;
        self.fit();
    }
    fn propose(&mut self, doc: DataDocument, path: Option<PathBuf>) {
        if self.dirty {
            self.pending = Some((doc, path));
        } else {
            self.apply(doc, path);
        }
    }
    pub fn open(&mut self) {
        if let Some(path) = rfd::FileDialog::new()
            .add_filter("Поток данных", &["json"])
            .pick_file()
        {
            match fs::read_to_string(&path)
                .map_err(anyhow::Error::from)
                .and_then(|s| DataDocument::decode(&s))
            {
                Ok(doc) => self.propose(doc, Some(path)),
                Err(error) => self.message = Some(format!("Не удалось открыть поток: {error}")),
            }
        }
    }
    pub fn save(&mut self) -> bool {
        let path = self.path.clone().or_else(|| {
            rfd::FileDialog::new()
                .add_filter("Поток данных", &["json"])
                .set_file_name("pipeline.json")
                .save_file()
        });
        let Some(path) = path else {
            return false;
        };
        match self
            .document
            .encode()
            .and_then(|s| fs::write(&path, s).map_err(anyhow::Error::from))
        {
            Ok(()) => {
                self.path = Some(path);
                self.dirty = false;
                self.message = None;
                true
            }
            Err(error) => {
                self.message = Some(format!("Не удалось сохранить поток: {error}"));
                false
            }
        }
    }
    pub fn toolbar(&mut self, ui: &mut egui::Ui) {
        if ui
            .button("Открыть…")
            .on_hover_text("Открыть поток Peregon / ppduster (⌘O)")
            .clicked()
        {
            self.open();
        }
        if ui
            .button("Сохранить")
            .on_hover_text("Сохранить поток (⌘S)")
            .clicked()
        {
            self.save();
        }
        if ui.button("Новый").clicked() {
            let mut document = DataDocument::default();
            document.graph.nodes.clear();
            document.graph.connections.clear();
            self.propose(document, None);
        }
        ui.add_sized(
            [120.0, 24.0],
            egui::Label::new(RichText::new(&self.document.graph.name).strong()).truncate(),
        )
        .on_hover_text(&self.document.graph.name);
        if self.dirty {
            ui.label(RichText::new("●").color(ORANGE))
                .on_hover_text("Есть несохранённые изменения");
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if ui
                .add_enabled(
                    self.receiver.is_none(),
                    egui::Button::new(
                        RichText::new(if self.receiver.is_some() {
                            "Выполняется…"
                        } else {
                            "▶ Выполнить"
                        })
                        .color(Color32::WHITE),
                    )
                    .fill(INK),
                )
                .clicked()
            {
                self.run(ui.ctx());
            }
            ui.label(RichText::new("Локально").color(CYAN))
                .on_hover_text("Данные обрабатываются на этом компьютере");
        });
    }
    fn run(&mut self, ctx: &egui::Context) {
        match self.document.graph.compile() {
            Ok(request) => {
                let revision = self.document.graph.revision;
                let (sender, receiver) = mpsc::channel();
                let ctx = ctx.clone();
                self.receiver = Some(receiver);
                self.message = None;
                std::thread::spawn(move || {
                    let output = ppduster::data_pipeline::process_request(&request.to_string());
                    let value = serde_json::from_str(&output).unwrap_or(
                        json!({"ok":false,"diagnostics":[{"message":"Некорректный ответ движка"}]}),
                    );
                    let _ = sender.send((revision, value));
                    ctx.request_repaint();
                });
            }
            Err(error) => {
                self.message = Some(error.to_string());
                self.tab = 2;
            }
        }
    }
    pub fn poll(&mut self) {
        if let Some(receiver) = &self.receiver {
            match receiver.try_recv() {
                Ok((revision, result)) => {
                    self.result = result;
                    self.result_revision = Some(revision);
                    self.receiver = None;
                    if self.result["ok"] == false {
                        self.tab = 2;
                    }
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.receiver = None;
                    self.message = Some("Выполнение прервано".into());
                }
                _ => {}
            }
        }
    }
    pub fn confirmation(&mut self, ctx: &egui::Context) {
        if self.pending.is_none() && !self.close_pending {
            return;
        }
        let mut save = false;
        let mut discard = false;
        let mut cancel = false;
        egui::Modal::new(Id::new("data-unsaved-confirmation")).show(ctx, |ui| {
            ui.set_width(400.);
            ui.heading("Сохранить изменения потока?");
            ui.label("В потоке данных есть несохранённые изменения.");
            ui.horizontal(|ui| {
                save = ui.button("Сохранить").clicked();
                discard = ui.button("Не сохранять").clicked();
                cancel = ui.button("Отмена").clicked();
            });
        });
        if cancel {
            self.pending = None;
            self.close_pending = false;
        }
        if discard || (save && self.save()) {
            self.dirty = false;
            if let Some((doc, path)) = self.pending.take() {
                self.apply(doc, path);
            }
            if self.close_pending {
                self.close_pending = false;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }
    pub fn show(&mut self, root: &mut egui::Ui, dark: bool) {
        self.results(root, dark);
        self.library(root, dark);
        self.inspector(root, dark);
        self.canvas(root, dark);
    }
    fn library(&mut self, root: &mut egui::Ui, dark: bool) {
        egui::Panel::left("data-library")
            .default_size(220.)
            .size_range(180. ..=320.)
            .resizable(true)
            .frame(
                Frame::new()
                    .fill(surface(dark))
                    .stroke(Stroke::new(1., line(dark)))
                    .corner_radius(16)
                    .outer_margin(10)
                    .inner_margin(14),
            )
            .show(root, |ui| {
                ui.label(RichText::new("БИБЛИОТЕКА БЛОКОВ").size(10.).color(MUTED));
                ui.add_space(10.);
                ui.add(egui::TextEdit::singleline(&mut self.search).hint_text("Найти блок…"));
                ui.add_space(10.);
                ScrollArea::vertical().show(ui, |ui| {
                    ui.collapsing("Начать с примера", |ui| {
                        for (index, label) in
                            ["JSON → CSV", "Фильтр активных → JSON", "Список → шаблон"]
                                .iter()
                                .enumerate()
                        {
                            if ui.button(*label).clicked() {
                                self.propose(preset(index), None);
                            }
                        }
                    });
                    ui.add_space(8.);
                    let search = self.search.to_lowercase();
                    for category in ["Источник", "Преобразование", "Результат"]
                    {
                        ui.label(RichText::new(category).strong());
                        for (kind, title, group) in BLOCKS.iter().filter(|(_, title, group)| {
                            *group == category && title.to_lowercase().contains(&search)
                        }) {
                            let _ = group;
                            if ui
                                .add(
                                    egui::Button::new(block_label(kind, title, dark))
                                        .min_size(Vec2::new(ui.available_width(), 32.)),
                                )
                                .clicked()
                            {
                                self.add(kind, title);
                            }
                        }
                        ui.add_space(16.);
                    }
                    ui.separator();
                    ui.label(RichText::new("ПОТОК").size(10.).color(MUTED));
                    let mut name = self.document.graph.name.clone();
                    if ui.text_edit_singleline(&mut name).changed() {
                        self.document.graph.name = name;
                        self.changed();
                    }
                    ui.add_space(8.);
                    let rows = self
                        .document
                        .graph
                        .nodes
                        .iter()
                        .map(|n| (n.id.clone(), n.title().to_owned()))
                        .collect::<Vec<_>>();
                    for (id, title) in rows {
                        if ui
                            .selectable_label(self.selected.as_ref() == Some(&id), title)
                            .clicked()
                        {
                            self.select(id);
                        }
                    }
                });
            });
    }
    fn add(&mut self, kind: &str, title: &str) {
        let mut number = self.document.graph.nodes.len() + 1;
        while self
            .document
            .graph
            .nodes
            .iter()
            .any(|n| n.id == format!("node-{number}"))
        {
            number += 1;
        }
        let id = format!("node-{number}");
        let position = self
            .selected
            .as_ref()
            .and_then(|id| self.document.graph.nodes.iter().find(|n| &n.id == id))
            .map(|n| DataPosition {
                x: n.position.x + 270.,
                y: n.position.y + 100.,
            })
            .unwrap_or(DataPosition { x: 40., y: 80. });
        let config = if kind.starts_with("source.") {
            json!({"title":title,"text":"","arrayPath":"","delimiter":","})
        } else if kind == "transform.project" {
            json!({"title":title,"fields":[]})
        } else if kind == "transform.filter" {
            json!({"title":title,"mode":"all","conditions":[]})
        } else {
            json!({"title":title,"template":"{value}","delimiter":if kind=="sink.csv"{","}else{"\n"},"includeHeader":true,"skipEmpty":true,"unique":false})
        };
        self.document.graph.nodes.push(DataNode {
            id: id.clone(),
            kind: kind.into(),
            version: 1,
            position,
            config,
        });
        self.select(id);
        self.changed();
    }
    fn inspector(&mut self, root: &mut egui::Ui, dark: bool) {
        egui::Panel::right("data-inspector")
            .default_size(310.)
            .size_range(260. ..=450.)
            .resizable(true)
            .frame(Frame::new().fill(surface(dark)).stroke(Stroke::new(1.,line(dark))).corner_radius(16).outer_margin(10).inner_margin(14))
            .show(root, |ui| {
                ui.label(RichText::new("НАСТРОЙКИ БЛОКА").size(10.).color(MUTED));
                ui.add_space(10.);
                let Some(index) = self
                    .selected
                    .as_ref()
                    .and_then(|id| self.document.graph.nodes.iter().position(|n| &n.id == id))
                else {
                    ui.label("Выберите блок на графе");
                    return;
                };
                let mut node = self.document.graph.nodes[index].clone();
                let before = node.config.clone();
                ScrollArea::vertical()
                    .id_salt("data-settings-scroll")
                    .show(ui, |ui| {
                        edit_text(ui, &mut node.config, "title", "Название", false);
                        ui.label(RichText::new(BLOCKS.iter().find(|(kind,_,_)|*kind==node.kind).map(|(_,title,category)|format!("{category} · {title}")).unwrap_or_default()).size(10.).color(MUTED));
                        ui.separator();
                        if !node.is_source() {
                            ui.label("Входные данные");
                            let incoming = self
                                .document
                                .graph
                                .connections
                                .iter()
                                .find(|e| e.to.node_id == node.id)
                                .map(|e| e.from.node_id.clone());
                            let mut input = incoming.clone().unwrap_or_default();
                            let title = incoming
                                .as_ref()
                                .and_then(|id| {
                                    self.document.graph.nodes.iter().find(|n| &n.id == id)
                                })
                                .map(|n| n.title())
                                .unwrap_or("Выберите предыдущий блок");
                            egui::ComboBox::from_id_salt("data-input")
                                .selected_text(title)
                                .width(ui.available_width())
                                .show_ui(ui, |ui| {
                                    for candidate in self
                                        .document
                                        .graph
                                        .nodes
                                        .iter()
                                        .filter(|n| n.id != node.id && !n.is_sink())
                                    {
                                        ui.selectable_value(
                                            &mut input,
                                            candidate.id.clone(),
                                            candidate.title(),
                                        );
                                    }
                                });
                            if incoming.as_deref() != Some(&input) && !input.is_empty() {
                                match self.document.graph.connect(&input, &node.id) {
                                    Ok(()) => self.changed(),
                                    Err(error) => self.message = Some(error.to_string()),
                                }
                            }
                            if incoming.is_some() && ui.small_button("Отсоединить вход").clicked()
                            {
                                self.document
                                    .graph
                                    .connections
                                    .retain(|e| e.to.node_id != node.id);
                                self.changed();
                            }
                            ui.separator();
                        }
                        if node.is_source() {
                            if ui.button("Загрузить из файла…").clicked() {
                                if let Some(path) = rfd::FileDialog::new().pick_file() {
                                    match fs::read_to_string(path) {
                                        Ok(s) => {let key=effective_key(&node.config,"text");node.config[&key] = json!(s);},
                                        Err(e) => self.message = Some(e.to_string()),
                                    }
                                }
                            }
                            edit_text(ui, &mut node.config, "text", "Исходные данные", true);
                            if node.kind != "source.list" {
                                let text_key=effective_key(&node.config,"text");
                                let source=node.config[&text_key].as_str().unwrap_or("").to_owned();
                                if ui.button("Найти наборы данных").clicked() {
                                    let request=json!({"action":"analyze","json":source,"source_format":node.kind.trim_start_matches("source."),"csv_delimiter":node.config["delimiter"].as_str().unwrap_or(",")});
                                    let response:Value=serde_json::from_str(&ppduster::data_pipeline::process_request(&request.to_string())).unwrap_or_default();
                                    if response["ok"]==true {self.arrays.insert(node.id.clone(),(source.clone(),response["array_paths"].as_array().cloned().unwrap_or_default()));}
                                    else {self.message=Some(response["error"]["message"].as_str().unwrap_or("Не удалось прочитать данные").to_owned());self.tab=2;}
                                }
                                let key=effective_key(&node.config,"arrayPath");
                                let mut path=node.config[&key].as_str().unwrap_or("").to_owned();let before=path.clone();
                                if let Some((analyzed,choices))=self.arrays.get(&node.id).filter(|(data,_)|data==&source) {
                                    let _=analyzed;
                                    ui.label("Набор данных");
                                    egui::ComboBox::from_id_salt("source-array").selected_text(if path.is_empty(){"Корневой массив"}else{&path}).width(ui.available_width()).show_ui(ui,|ui|{
                                        for choice in choices {if let Some(value)=choice["path"].as_str(){ui.selectable_value(&mut path,value.to_owned(),format!("{} · {} строк",if value.is_empty(){"Корень"}else{value},choice["length"].as_u64().or_else(||choice["items"].as_u64()).unwrap_or(0)));}}
                                    });
                                }
                                if before!=path {node.config[&key]=json!(path);}
                                ui.collapsing("Путь вручную",|ui|{edit_text(ui,&mut node.config,"arrayPath","Путь вложенного массива, например /stores",false);});
                            }
                            if node.kind == "source.csv" {
                                edit_text(
                                    ui,
                                    &mut node.config,
                                    "delimiter",
                                    "Разделитель CSV",
                                    false,
                                );
                            }
                        } else if node.kind == "transform.project" {
                            ui.label("Поля и порядок результата");
                            let mut fields = node.config["fields"]
                                .as_array()
                                .cloned()
                                .unwrap_or_default();
                            let available = self.result["nodes"][&node.id]["input_schema"]
                                ["fields"]
                                .as_array()
                                .cloned()
                                .unwrap_or_default();
                            for field in available {
                                if let Some(name) = field["name"].as_str() {
                                    let mut checked =
                                        fields.iter().any(|f| f.as_str() == Some(name));
                                    if ui.checkbox(&mut checked, name).changed() {
                                        if checked {
                                            fields.push(json!(name));
                                        } else {
                                            fields.retain(|f| f.as_str() != Some(name));
                                        }
                                    }
                                }
                            }
                            let mut names = fields
                                .iter()
                                .filter_map(Value::as_str)
                                .collect::<Vec<_>>()
                                .join(", ");
                            if ui.text_edit_singleline(&mut names).changed() {
                                fields = names
                                    .split(',')
                                    .map(str::trim)
                                    .filter(|s| !s.is_empty())
                                    .map(|s| json!(s))
                                    .collect();
                            }
                            let mut move_field=None;
                        for (index,field) in fields.iter().enumerate(){ui.horizontal(|ui|{
                            ui.label(field.as_str().unwrap_or(""));
                            if ui.add_enabled(index>0,egui::Button::new("↑").small()).clicked(){move_field=Some((index,index-1));}
                            if ui.add_enabled(index+1<fields.len(),egui::Button::new("↓").small()).clicked(){move_field=Some((index,index+1));}
                        });}
                        if let Some((from,to))=move_field{fields.swap(from,to);}
                        node.config["fields"] = json!(fields);
                            if self.result.is_null() {
                                ui.label(
                                    RichText::new("Выполните поток, чтобы увидеть доступные поля.")
                                        .size(10.)
                                        .color(MUTED),
                                );
                            }
                        } else if node.kind == "transform.filter" {
                            edit_filter(ui, &mut node.config);
                        } else {
                            if node.kind == "transform.template" || node.kind == "sink.template" {
                                edit_text(
                                    ui,
                                    &mut node.config,
                                    "template",
                                    "Шаблон · {value}",
                                    false,
                                );
                            }
                            if matches!(
                                node.kind.as_str(),
                                "sink.csv" | "sink.flat" | "sink.template" | "sink.join"
                            ) {
                                edit_text(ui, &mut node.config, "delimiter", "Разделитель", false);
                            }
                            if node.kind == "sink.csv" {
                                edit_bool(
                                    ui,
                                    &mut node.config,
                                    "includeHeader",
                                    "Заголовок столбцов",
                                    true,
                                );
                                edit_bool(
                                    ui,
                                    &mut node.config,
                                    "quoteAll",
                                    "Все значения в кавычках",
                                    false,
                                );
                            }
                            if node.kind == "sink.xml" {
                                edit_text(ui, &mut node.config, "root", "Корневой тег", false);
                                edit_text(ui, &mut node.config, "row", "Тег строки", false);
                            }
                            if node.kind == "sink.sql" {
                                edit_text(ui, &mut node.config, "table", "Название таблицы", false);
                            }
                            if matches!(
                                node.kind.as_str(),
                                "sink.flat" | "sink.template" | "sink.join" | "transform.template"
                            ) {
                                edit_bool(
                                    ui,
                                    &mut node.config,
                                    "skipEmpty",
                                    "Пропускать пустые",
                                    true,
                                );
                                edit_bool(
                                    ui,
                                    &mut node.config,
                                    "unique",
                                    "Убирать дубликаты",
                                    false,
                                );
                            }
                        }
                        ui.separator();
                        ui.collapsing("Дополнительные настройки", |ui| {
                            if self.config_text.is_empty() {
                                self.config_text =
                                    serde_json::to_string_pretty(&node.config).unwrap_or_default();
                            }
                            ui.add(
                                egui::TextEdit::multiline(&mut self.config_text)
                                    .code_editor()
                                    .desired_rows(7)
                                    .desired_width(f32::INFINITY),
                            );
                            if ui.button("Применить настройки").clicked() {
                                match serde_json::from_str::<Value>(&self.config_text) {
                                    Ok(config) if config.is_object() => node.config = config,
                                    _ => {
                                        self.message =
                                            Some("Настройки должны быть JSON-объектом".into())
                                    }
                                }
                            }
                        });
                        ui.add_space(12.);
                        if ui
                            .button(RichText::new("Удалить блок").color(ORANGE))
                            .clicked()
                        {
                            self.document.graph.nodes.remove(index);
                            self.document
                                .graph
                                .connections
                                .retain(|e| e.from.node_id != node.id && e.to.node_id != node.id);
                            self.selected = None;
                            self.changed();
                            return;
                        }
                    });
                if self
                    .document
                    .graph
                    .nodes
                    .get(index)
                    .is_some_and(|n| n.id == node.id)
                    && before != node.config
                {
                    self.document.graph.nodes[index] = node;
                    self.config_text.clear();
                    self.changed();
                }
            });
    }
    fn fit(&mut self) {
        let mut bounds = Rect::NOTHING;
        for node in &self.document.graph.nodes {
            bounds = bounds.union(node_rect(node));
        }
        if bounds.is_positive() {
            self.scene = bounds.expand(60.);
        }
    }
    fn canvas(&mut self, root: &mut egui::Ui, dark: bool) {
        egui::CentralPanel::default()
            .frame(Frame::new().fill(canvas(dark)))
            .show(root, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("ПОТОК ДАННЫХ").size(10.).color(MUTED));
                    ui.label(format!(
                        "{} блоков · {} связей",
                        self.document.graph.nodes.len(),
                        self.document.graph.connections.len()
                    ));
                    if ui.small_button("Показать весь граф").clicked() {
                        self.fit();
                    }
                    if let Some(id) = &self.connecting {
                        ui.label(RichText::new(format!("Выберите вход · {id}")).color(CYAN));
                        if ui.small_button("Отмена").clicked() {
                            self.connecting = None;
                        }
                    }
                });
                let mut scene = self.scene;
                let mut selected = None;
                let mut connect = None;
                let mut changed = false;
                let mut background_delta = Vec2::ZERO;
                egui::Scene::new()
                    .zoom_range(0.3..=1.5)
                    .max_inner_size(Vec2::splat(100_000.))
                    .sense(Sense::hover())
                    .drag_pan_buttons(egui::DragPanButtons::empty())
                    .show(ui, &mut scene, |ui| {
                        let background = ui.interact(
                            ui.clip_rect(),
                            Id::new("data-canvas-background"),
                            Sense::click_and_drag(),
                        );
                        let mut card_dragged = false;
                        let painter = ui.painter().clone();
                        paint_grid(&painter, ui.clip_rect(), dark);
                        for edge in &self.document.graph.connections {
                            if let (Some(from), Some(to)) = (
                                self.document
                                    .graph
                                    .nodes
                                    .iter()
                                    .find(|n| n.id == edge.from.node_id),
                                self.document
                                    .graph
                                    .nodes
                                    .iter()
                                    .find(|n| n.id == edge.to.node_id),
                            ) {
                                let a = node_rect(from).right_center();
                                let b = node_rect(to).left_center();
                                let offset = Vec2::new((b.x - a.x).abs().max(90.) * 0.5, 0.);
                                painter.add(egui::epaint::CubicBezierShape::from_points_stroke(
                                    [a, a + offset, b - offset, b],
                                    false,
                                    Color32::TRANSPARENT,
                                    Stroke::new(2., translucent(node_color(from), 170)),
                                ));
                            }
                        }
                        for node in &mut self.document.graph.nodes {
                            let rect = node_rect(node);
                            let color = node_color(node);
                            painter.rect_filled(
                                rect.translate(Vec2::new(0., 4.)),
                                16.,
                                translucent(Color32::BLACK, if dark { 20 } else { 10 }),
                            );
                            painter.rect(
                                rect,
                                16.,
                                card(dark),
                                Stroke::new(
                                    if self.selected.as_ref() == Some(&node.id) {
                                        1.8
                                    } else {
                                        1.
                                    },
                                    if self.selected.as_ref() == Some(&node.id) {
                                        translucent(color, 180)
                                    } else {
                                        line(dark)
                                    },
                                ),
                                StrokeKind::Inside,
                            );
                            painter.rect_filled(
                                Rect::from_min_size(
                                    rect.min + Vec2::new(0., 14.),
                                    Vec2::new(3., rect.height() - 28.),
                                ),
                                2.,
                                color,
                            );
                            let icon = Rect::from_min_size(
                                rect.min + Vec2::new(14., 16.),
                                Vec2::splat(32.),
                            );
                            painter.rect_filled(icon, 9., color);
                            painter.text(
                                icon.center(),
                                Align2::CENTER_CENTER,
                                node_icon(node),
                                FontId::proportional(13.),
                                Color32::WHITE,
                            );
                            let response = ui.interact(
                                rect.shrink2(Vec2::new(14., 4.)),
                                Id::new(("data-node", &node.id)),
                                Sense::click_and_drag(),
                            );
                            if response.clicked() {
                                selected = Some(node.id.clone());
                            }
                            if response.dragged() {
                                card_dragged = true;
                                let delta = response.drag_delta();
                                node.position.x += delta.x;
                                node.position.y += delta.y;
                                changed = true;
                            }
                            painter.text(
                                rect.min + Vec2::new(58., 16.),
                                Align2::LEFT_TOP,
                                if node.is_source() {
                                    "ИСТОЧНИК"
                                } else if node.is_sink() {
                                    "РЕЗУЛЬТАТ"
                                } else {
                                    "ПРЕОБРАЗОВАНИЕ"
                                },
                                FontId::proportional(9.),
                                color,
                            );
                            let title = node.title().chars().take(22).collect::<String>();
                            painter.text(
                                rect.min + Vec2::new(58., 33.),
                                Align2::LEFT_TOP,
                                title,
                                FontId::proportional(12.),
                                text(dark),
                            );
                            painter.line_segment(
                                [
                                    rect.min + Vec2::new(14., 65.),
                                    rect.min + Vec2::new(rect.width() - 14., 65.),
                                ],
                                Stroke::new(1., line(dark)),
                            );
                            let summary = node_summary(node);
                            painter.text(
                                rect.min + Vec2::new(16., 82.),
                                Align2::LEFT_TOP,
                                summary,
                                FontId::proportional(10.),
                                text(dark),
                            );
                            let stats = &self.result["nodes"][&node.id];
                            let status = if self
                                .result_revision
                                .is_some_and(|r| r != self.document.graph.revision)
                            {
                                "Изменён · выполните снова".into()
                            } else if stats["ok"] == false {
                                "Ошибка · см. результат".into()
                            } else if let Some(rows) = stats["stats"]["output_items"].as_u64() {
                                format!("{rows} строк · готово")
                            } else {
                                node.kind.clone()
                            };
                            painter.text(
                                rect.min + Vec2::new(16., 115.),
                                Align2::LEFT_TOP,
                                status,
                                FontId::proportional(10.),
                                MUTED,
                            );
                            if !node.is_source() {
                                let point = rect.left_center();
                                painter.circle_filled(point, 8., card(dark));
                                painter.circle_stroke(
                                    point,
                                    8.,
                                    Stroke::new(1.4, translucent(color, 110)),
                                );
                                painter.circle_filled(point, 4., color);
                                if ui
                                    .interact(
                                        Rect::from_center_size(point, Vec2::splat(22.)),
                                        Id::new(("data-input-port", &node.id)),
                                        Sense::click(),
                                    )
                                    .clicked()
                                {
                                    if let Some(from) = self.connecting.take() {
                                        connect = Some((from, node.id.clone()));
                                    }
                                }
                            }
                            if !node.is_sink() {
                                let point = rect.right_center();
                                painter.circle_filled(point, 8., card(dark));
                                painter.circle_stroke(
                                    point,
                                    8.,
                                    Stroke::new(1.4, translucent(color, 110)),
                                );
                                painter.circle_filled(point, 4., color);
                                if ui
                                    .interact(
                                        Rect::from_center_size(point, Vec2::splat(22.)),
                                        Id::new(("data-output-port", &node.id)),
                                        Sense::click(),
                                    )
                                    .clicked()
                                {
                                    self.connecting = Some(node.id.clone());
                                }
                            }
                        }
                        if background.dragged() && !card_dragged {
                            background_delta = background.drag_delta();
                        }
                        ui.expand_to_include_rect(
                            self.document
                                .graph
                                .nodes
                                .iter()
                                .fold(Rect::NOTHING, |r, n| r.union(node_rect(n)))
                                .expand(60.),
                        );
                    });
                self.scene = scene.translate(-background_delta);
                if let Some(id) = selected {
                    self.select(id);
                }
                if changed {
                    self.changed();
                }
                if let Some((from, to)) = connect {
                    match self.document.graph.connect(&from, &to) {
                        Ok(()) => {
                            self.changed();
                            self.message = None;
                        }
                        Err(e) => self.message = Some(e.to_string()),
                    }
                }
            });
    }
    fn results(&mut self, root: &mut egui::Ui, dark: bool) {
        egui::Panel::bottom("data-results")
            .default_size(230.)
            .size_range(120. ..=420.)
            .resizable(true)
            .frame(
                Frame::new()
                    .fill(surface(dark))
                    .stroke(Stroke::new(1., line(dark)))
                    .corner_radius(16)
                    .outer_margin(10)
                    .inner_margin(12),
            )
            .show(root, |ui| {
                ui.horizontal(|ui| {
                    for (index, title) in ["Результат", "Предпросмотр блока", "Проблемы"]
                        .iter()
                        .enumerate()
                    {
                        ui.selectable_value(&mut self.tab, index, *title);
                    }
                    if self
                        .result_revision
                        .is_some_and(|r| r != self.document.graph.revision)
                    {
                        ui.label(
                            RichText::new("Данные изменились · результат предыдущего запуска")
                                .color(ORANGE),
                        );
                    }
                });
                ui.separator();
                if self.tab != 2 {
                    if let Some(message) = &self.message {
                        ui.label(RichText::new(message).color(ORANGE));
                    }
                }
                if self.tab == 2 {
                    ScrollArea::vertical().show(ui, |ui| {
                        if let Some(message) = &self.message {
                            ui.label(RichText::new(message).color(ORANGE));
                        }
                        if let Some(issues) = self.result["diagnostics"].as_array() {
                            for issue in issues {
                                ui.label(
                                    RichText::new(issue["message"].as_str().unwrap_or("Ошибка"))
                                        .color(ORANGE),
                                );
                            }
                        }
                        if self.message.is_none()
                            && self.result["diagnostics"]
                                .as_array()
                                .is_none_or(Vec::is_empty)
                        {
                            ui.label("Проблем нет");
                        }
                    });
                    return;
                }
                let mut content = if self.tab == 1 {
                    self.selected
                        .as_ref()
                        .map(|id| {
                            serde_json::to_string_pretty(&self.result["nodes"][id]["preview"])
                                .unwrap_or_default()
                        })
                        .unwrap_or_default()
                } else {
                    let outputs = self.result["sink_outputs"].as_object();
                    outputs
                        .and_then(|o| {
                            self.selected
                                .as_ref()
                                .and_then(|id| o.get(id))
                                .or_else(|| o.values().next())
                        })
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned()
                };
                if content.is_empty() || content == "null" {
                    ui.label(
                        RichText::new(
                            "Выполните поток. Здесь появятся данные выбранного блока и результат.",
                        )
                        .color(MUTED),
                    );
                    return;
                }
                ui.horizontal(|ui| {
                    if ui.button("Копировать").clicked() {
                        ui.ctx().copy_text(content.clone());
                    }
                    if ui.button("Сохранить результат…").clicked() {
                        if let Some(path) = rfd::FileDialog::new()
                            .set_file_name("result.txt")
                            .save_file()
                        {
                            if let Err(e) = fs::write(path, &content) {
                                self.message = Some(e.to_string());
                                self.tab = 2;
                            }
                        }
                    }
                });
                ScrollArea::both().show(ui, |ui| {
                    ui.add(
                        egui::TextEdit::multiline(&mut content)
                            .code_editor()
                            .interactive(false)
                            .background_color(Color32::from_rgb(23, 29, 31))
                            .text_color(Color32::from_rgb(224, 230, 223))
                            .desired_width(f32::INFINITY),
                    );
                });
            });
    }
}
fn node_rect(node: &DataNode) -> Rect {
    Rect::from_min_size(
        Pos2::new(node.position.x, node.position.y),
        Vec2::new(240., 148.),
    )
}
fn edit_text(ui: &mut egui::Ui, config: &mut Value, key: &str, label: &str, multiline: bool) {
    ui.label(label);
    let key = effective_key(config, key);
    let mut value = config[&key].as_str().unwrap_or("").to_owned();
    let mut layouter = |ui: &egui::Ui, buffer: &dyn egui::TextBuffer, width: f32| {
        let mut job = data_code_layout(buffer.as_str());
        job.wrap.max_width = width;
        ui.fonts_mut(|fonts| fonts.layout_job(job))
    };
    let response = if multiline {
        ui.add(
            egui::TextEdit::multiline(&mut value)
                .code_editor()
                .desired_rows(10)
                .background_color(Color32::from_rgb(23, 29, 31))
                .text_color(Color32::from_rgb(224, 230, 223))
                .layouter(&mut layouter)
                .desired_width(f32::INFINITY),
        )
    } else {
        ui.add(egui::TextEdit::singleline(&mut value).desired_width(f32::INFINITY))
    };
    if response.changed() {
        config[&key] = json!(value);
    }
    ui.add_space(8.);
}
fn edit_bool(ui: &mut egui::Ui, config: &mut Value, key: &str, label: &str, default: bool) {
    let key = effective_key(config, key);
    let mut value = config[&key].as_bool().unwrap_or(default);
    if ui.checkbox(&mut value, label).changed() {
        config[&key] = json!(value);
    }
}
fn effective_key(config: &Value, key: &str) -> String {
    let aliases: &[&str] = match key {
        "text" => &["data", "json", "text"],
        "arrayPath" => &["path", "arrayPath"],
        "template" => &["value_template", "valueTemplate", "template"],
        "skipEmpty" => &["skip_empty", "skipEmpty"],
        "includeHeader" => &["csv_include_header", "csvIncludeHeader", "includeHeader"],
        "quoteAll" => &["csv_quote_all", "csvQuoteAll", "quoteAll"],
        "root" => &["xml_root", "xmlRoot", "root"],
        "row" => &["xml_row", "xmlRow", "row"],
        "table" => &["table_name", "tableName", "table"],
        _ => &[],
    };
    aliases
        .iter()
        .find(|alias| config.get(**alias).is_some())
        .copied()
        .unwrap_or(key)
        .to_owned()
}
fn edit_filter(ui: &mut egui::Ui, config: &mut Value) {
    let mut expression=config.get("expression").filter(|v|!v.is_null()).cloned().unwrap_or_else(|| {
        let children=config["conditions"].as_array().or_else(||config["filters"].as_array()).cloned().unwrap_or_default().into_iter().map(|mut condition|{condition["kind"]=json!("condition");condition}).collect::<Vec<_>>();
        json!({"kind":"group","operator":if config["mode"]=="any" || config["filter_mode"]=="any"{"or"}else{"and"},"children":children})
    });
    let before = expression.clone();
    edit_expression(ui, &mut expression, 0);
    if before != expression {
        config["expression"] = expression;
    }
}
fn edit_expression(ui: &mut egui::Ui, expression: &mut Value, depth: usize) {
    if depth >= 32 {
        ui.label("Достигнут предел вложенности условий");
        return;
    }
    match expression["kind"].as_str().unwrap_or("") {
        "group" => {
            let mut operator = expression["operator"].as_str().unwrap_or("and").to_owned();
            ui.horizontal(|ui| {
                ui.selectable_value(&mut operator, "and".into(), "Все условия · И");
                ui.selectable_value(&mut operator, "or".into(), "Любое · ИЛИ");
            });
            expression["operator"] = json!(operator);
            let mut children = expression["children"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            let mut remove = None;
            for (index, child) in children.iter_mut().enumerate() {
                ui.push_id(index, |ui| {
                    Frame::new().inner_margin(8).show(ui, |ui| {
                        edit_expression(ui, child, depth + 1);
                        if ui.small_button("Удалить условие / группу").clicked()
                        {
                            remove = Some(index);
                        }
                    });
                });
            }
            if let Some(index) = remove {
                children.remove(index);
            }
            ui.horizontal_wrapped(|ui|{
                if ui.small_button("+ Условие").clicked(){children.push(json!({"kind":"condition","field":"name","operator":"equal","value":"","quantifier":"one"}));}
                if ui.small_button("+ Группа").clicked(){children.push(json!({"kind":"group","operator":"and","children":[]}));}
                if ui.small_button("+ НЕ").clicked(){children.push(json!({"kind":"not","child":{"kind":"condition","field":"name","operator":"exists","value":"","quantifier":"one"}}));}
            });
            expression["children"] = json!(children);
        }
        "not" => {
            ui.label(RichText::new("НЕ").strong());
            ui.indent("negated-rule", |ui| {
                edit_expression(ui, &mut expression["child"], depth + 1);
            });
        }
        "condition" => {
            edit_text(ui, expression, "field", "Поле или вложенный путь", false);
            let mut quantifier = expression["quantifier"]
                .as_str()
                .unwrap_or("one")
                .to_owned();
            let previous = quantifier.clone();
            egui::ComboBox::from_id_salt("quantifier")
                .selected_text(match quantifier.as_str() {
                    "any" => "Любое значение",
                    "all" => "Все значения",
                    "none" => "Ни одного",
                    _ => "Одно значение",
                })
                .show_ui(ui, |ui| {
                    for (value, label) in [
                        ("one", "Одно значение"),
                        ("any", "Любое значение"),
                        ("all", "Все значения"),
                        ("none", "Ни одного"),
                    ] {
                        ui.selectable_value(&mut quantifier, value.to_owned(), label);
                    }
                });
            if previous != quantifier {
                expression["quantifier"] = json!(quantifier);
            }
            let mut operator = expression["operator"]
                .as_str()
                .unwrap_or("equal")
                .to_owned();
            egui::ComboBox::from_id_salt("operator")
                .selected_text(operator_label(&operator))
                .show_ui(ui, |ui| {
                    for (value, label) in OPERATORS {
                        ui.selectable_value(&mut operator, (*value).to_owned(), *label);
                    }
                });
            expression["operator"] = json!(operator);
            if !matches!(
                expression["operator"].as_str(),
                Some("exists" | "not_exists")
            ) {
                edit_text(ui, expression, "value", "Значение", false);
            }
        }
        _ => {
            ui.label("Неподдерживаемое выражение · проверьте настройки");
        }
    }
}
const OPERATORS: &[(&str, &str)] = &[
    ("equal", "Равно"),
    ("not_equal", "Не равно"),
    ("greater_than", "Больше"),
    ("greater_or_equal", "Больше или равно"),
    ("less_than", "Меньше"),
    ("less_or_equal", "Меньше или равно"),
    ("contains", "Содержит"),
    ("starts_with", "Начинается с"),
    ("ends_with", "Заканчивается на"),
    ("exists", "Есть значение"),
    ("not_exists", "Нет значения"),
];
fn operator_label(value: &str) -> &str {
    OPERATORS
        .iter()
        .find(|(v, _)| *v == value)
        .map(|(_, label)| *label)
        .unwrap_or(value)
}

fn preset(index: usize) -> DataDocument {
    use ppduster::data_graph::edge;
    let mut doc = DataDocument::default();
    if index == 1 {
        doc.graph.name = "Активные записи".into();
        doc.graph.nodes.push(DataNode{id:"filter".into(),kind:"transform.filter".into(),version:1,position:DataPosition{x:310.,y:80.},config:json!({"title":"Только активные","expression":{"kind":"group","operator":"and","children":[{"kind":"condition","field":"active","operator":"equal","value":"true","quantifier":"one"}]}})});
        doc.graph.nodes[1].position.x = 580.;
        doc.graph.nodes[2].position.x = 850.;
        doc.graph.nodes[2].kind = "sink.json".into();
        doc.graph.nodes[2].config["title"] = json!("Результат JSON");
        doc.graph.connections = vec![
            edge("source", "records", "filter", "records"),
            edge("filter", "matched", "fields", "records"),
            edge("fields", "records", "output", "records"),
        ];
    } else if index == 2 {
        doc.graph.name = "Значения по шаблону".into();
        doc.graph.nodes[0].kind = "source.list".into();
        doc.graph.nodes[0].config = json!({"title":"Список значений","text":"a, b, c"});
        doc.graph.nodes[1].kind = "transform.template".into();
        doc.graph.nodes[1].config = json!({"title":"Шаблон значения","template":"<{value}>"});
        doc.graph.nodes[2].kind = "sink.join".into();
        doc.graph.nodes[2].config = json!({"title":"Объединить","delimiter":"\n"});
        doc.graph.connections = vec![
            edge("source", "values", "fields", "values"),
            edge("fields", "values", "output", "values"),
        ];
    }
    doc
}

fn node_color(node: &DataNode) -> Color32 {
    if node.is_source() {
        PURPLE
    } else if node.kind == "transform.filter" {
        CYAN
    } else if node.is_sink() {
        ORANGE
    } else {
        Color32::from_rgb(73, 118, 166)
    }
}
fn node_icon(node: &DataNode) -> &str {
    match node.kind.as_str() {
        "source.json" | "sink.json" => "{ }",
        "source.csv" | "sink.csv" => "CSV",
        "source.list" => "≡",
        "transform.filter" => "ƒ",
        "transform.project" => "⌗",
        "transform.template" | "sink.template" => "{x}",
        "sink.xml" => "XML",
        "sink.sql" => "SQL",
        _ => "→",
    }
}
fn node_summary(node: &DataNode) -> String {
    if node.is_source() {
        format!(
            "{} · {}",
            node.kind.trim_start_matches("source.").to_uppercase(),
            if node.config[effective_key(&node.config, "arrayPath")]
                .as_str()
                .unwrap_or("")
                .is_empty()
            {
                "Корневой набор"
            } else {
                "Вложенный набор"
            }
        )
    } else if node.kind == "transform.project" {
        node.config["fields"]
            .as_array()
            .map(|fields| {
                fields
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" · ")
            })
            .unwrap_or_else(|| "Выберите поля".into())
            .chars()
            .take(32)
            .collect()
    } else if node.kind == "transform.filter" {
        "Строки, соответствующие условиям".into()
    } else if node.kind == "transform.template" {
        node.config[effective_key(&node.config, "template")]
            .as_str()
            .unwrap_or("{value}")
            .chars()
            .take(32)
            .collect()
    } else {
        format!(
            "Экспорт · {}",
            BLOCKS
                .iter()
                .find(|(kind, _, _)| *kind == node.kind)
                .map(|(_, title, _)| *title)
                .unwrap_or("Текст")
        )
    }
}

fn data_code_layout(source: &str) -> LayoutJob {
    let base = Color32::from_rgb(224, 230, 223);
    let mut job = LayoutJob::default();
    let append = |job: &mut LayoutJob, text: &str, color: Color32| {
        job.append(
            text,
            0.,
            TextFormat {
                font_id: FontId::monospace(10.5),
                color,
                ..Default::default()
            },
        )
    };
    if source.len() > 128 * 1024 || !source.trim_start().starts_with(['{', '[']) {
        append(&mut job, source, base);
        return job;
    }
    let response: Value = serde_json::from_str(&ppduster::data_pipeline::process_request(
        &json!({"action":"tokenize_json","source":source}).to_string(),
    ))
    .unwrap_or_default();
    let mut offsets = vec![0];
    for (byte, ch) in source.char_indices() {
        for _ in 1..ch.len_utf16() {
            offsets.push(byte);
        }
        offsets.push(byte + ch.len_utf8());
    }
    let mut cursor = 0;
    if let Some(tokens) = response["tokens"].as_array() {
        for token in tokens {
            let from = token["from"]
                .as_u64()
                .and_then(|n| offsets.get(n as usize))
                .copied()
                .unwrap_or(cursor);
            let to = token["to"]
                .as_u64()
                .and_then(|n| offsets.get(n as usize))
                .copied()
                .unwrap_or(from);
            if from < cursor || to < from || to > source.len() {
                continue;
            }
            append(&mut job, &source[cursor..from], base);
            let color = match token["kind"].as_str() {
                Some("key") => Color32::from_rgb(129, 206, 212),
                Some("string") => Color32::from_rgb(198, 222, 133),
                Some("number") => Color32::from_rgb(210, 158, 214),
                Some("boolean" | "null") => Color32::from_rgb(224, 161, 111),
                Some("invalid") => Color32::from_rgb(245, 130, 130),
                _ => base,
            };
            append(&mut job, &source[from..to], color);
            cursor = to;
        }
    }
    append(&mut job, &source[cursor..], base);
    job
}

fn block_label(kind: &str, title: &str, dark: bool) -> LayoutJob {
    let node = DataNode {
        id: String::new(),
        kind: kind.into(),
        version: 1,
        position: DataPosition { x: 0., y: 0. },
        config: json!({}),
    };
    let mut job = LayoutJob::default();
    let color = node_color(&node);
    job.append(
        &format!(" {} ", node_icon(&node)),
        0.,
        TextFormat {
            font_id: FontId::proportional(11.),
            color: Color32::WHITE,
            background: color,
            ..Default::default()
        },
    );
    job.append(
        &format!("   {title}   +"),
        0.,
        TextFormat {
            font_id: FontId::proportional(11.),
            color: text(dark),
            ..Default::default()
        },
    );
    job
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_presets_execute_and_render_without_dirtying_the_document() {
        for index in 0..3 {
            let mut workspace = DataWorkspace::default();
            workspace.apply(preset(index), None);
            let request = workspace.document.graph.compile().unwrap();
            let result: Value = serde_json::from_str(&ppduster::data_pipeline::process_request(
                &request.to_string(),
            ))
            .unwrap();
            assert_eq!(result["ok"], true, "{result}");
            let ctx = egui::Context::default();
            configure_styles(&ctx, egui::ThemePreference::Light);
            let mut output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(980., 680.))),
                    ..Default::default()
                },
                |ui| {
                    workspace.show(ui, false);
                },
            );
            output.textures_delta.clear();
            assert!(
                !workspace.dirty,
                "Opening a preset must not change its configuration"
            );
        }
    }
    #[test]
    fn dragging_a_data_card_does_not_pan_the_canvas() {
        fn frame(ctx: &egui::Context, workspace: &mut DataWorkspace, events: Vec<egui::Event>) {
            let mut output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1440., 900.))),
                    events,
                    ..Default::default()
                },
                |ui| workspace.show(ui, false),
            );
            output.textures_delta.clear();
        }
        let ctx = egui::Context::default();
        let mut workspace = DataWorkspace::default();
        frame(&ctx, &mut workspace, vec![]);
        frame(&ctx, &mut workspace, vec![]);
        let response = ctx.read_response(Id::new(("data-node", "source"))).unwrap();
        let position = ctx
            .layer_transform_to_global(response.layer_id)
            .unwrap_or_default()
            * response.rect.center();
        let before = workspace.document.graph.nodes[0].position;
        let scene = workspace.scene;
        frame(
            &ctx,
            &mut workspace,
            vec![
                egui::Event::PointerMoved(position),
                egui::Event::PointerButton {
                    pos: position,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
        let end = position + Vec2::new(60., 0.);
        frame(&ctx, &mut workspace, vec![egui::Event::PointerMoved(end)]);
        frame(
            &ctx,
            &mut workspace,
            vec![egui::Event::PointerButton {
                pos: end,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            }],
        );
        assert!(workspace.document.graph.nodes[0].position.x > before.x);
        assert!((workspace.scene.min - scene.min).length() < 0.1);
        assert!(workspace.dirty);
    }
    #[test]
    fn source_highlighting_preserves_unicode_and_large_integer_text() {
        let text = r#"[{"город":"Тбилиси 🦀","id":9007199254740993}]"#;
        assert_eq!(data_code_layout(text).text, text);
    }
}
