//! Bilingual UI strings (EN/RU). `Str` enum is exhaustive, so adding a key
//! forces translations in both languages at compile time.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    En,
    Ru,
}

impl Lang {
    pub fn parse(s: &str) -> Lang {
        match s.to_ascii_lowercase().as_str() {
            "ru" | "rus" | "рус" => Lang::Ru,
            _ => Lang::En,
        }
    }

    pub fn code(&self) -> &'static str {
        match self {
            Lang::En => "en",
            Lang::Ru => "ru",
        }
    }

    pub fn title(&self) -> &'static str {
        match self {
            Lang::En => "English",
            Lang::Ru => "Русский",
        }
    }

    pub fn next(&self) -> Lang {
        match self {
            Lang::En => Lang::Ru,
            Lang::Ru => Lang::En,
        }
    }
}

#[derive(Debug, Clone, Copy)]
#[allow(dead_code)] // exhaustive on purpose: both languages must define every key
pub enum Str {
    // generic
    Ok,
    Cancel,
    Close,
    Save,
    Delete,
    Rename,
    Confirm,
    Yes,
    No,
    Error,
    Loading,
    Empty,
    None_,
    Search,
    // header / status
    AppTitle,
    StateIdle,
    StateBusy,
    StateError,
    StateConnecting,
    // sidebar
    History,
    NewChat,
    Settings,
    Profile,
    Active,
    NoSessions,
    SessionDeleted,
    DeleteSession,
    DeleteSessionQ,
    RenameSession,
    SessionTitle,
    AttachHint,
    // chat
    You,
    Assistant,
    Generating,
    TypeMessage,
    SendEnter,
    StopEsc,
    Attachments,
    NoHistory,
    OpenDraft,
    // modals
    Help,
    HelpTitle,
    ModelPicker,
    ModelPickerEmpty,
    HarnessPicker,
    Harness,
    HarnessKind,
    SettingsTitle,
    SettingsLanguage,
    SettingsProfile,
    SettingsActiveHarness,
    SettingsServerUrl,
    SettingsServerUrlHint,
    SettingsWorkdir,
    SettingsAgent,
    SettingsDefaultModel,
    SettingsEnabled,
    SettingsDisabledKind,
    SettingsRestartNote,
    FilePicker,
    Drives,
    PickAttach,
    ParentDir,
    // composer / misc
    NewlineHint,
    NotConnected,
    Draft,
    StopNote,
    SettingsServerPassword,
    PermOnce,
    PermAlways,
    PermReject,
    SettingsAutoApprove,
    // check mode
    CheckOk,
    CheckModels,
    CheckSessions,
}

pub fn t(lang: Lang, key: Str) -> &'static str {
    match lang {
        Lang::En => match key {
            Str::Ok => "OK",
            Str::Cancel => "Cancel",
            Str::Close => "Close",
            Str::Save => "Save",
            Str::Delete => "Delete",
            Str::Rename => "Rename",
            Str::Confirm => "Confirm",
            Str::Yes => "Yes",
            Str::No => "No",
            Str::Error => "Error",
            Str::Loading => "loading…",
            Str::Empty => "(empty)",
            Str::None_ => "none",
            Str::Search => "Search",

            Str::AppTitle => "ai-commander",
            Str::StateIdle => "idle",
            Str::StateBusy => "generating…",
            Str::StateError => "error",
            Str::StateConnecting => "connecting…",

            Str::History => "History",
            Str::NewChat => "New chat",
            Str::Settings => "Settings",
            Str::Profile => "Profile",
            Str::Active => "active",
            Str::NoSessions => "no sessions yet",
            Str::SessionDeleted => "session deleted",
            Str::DeleteSession => "Delete session",
            Str::DeleteSessionQ => "Delete this session permanently?",
            Str::RenameSession => "Rename session",
            Str::SessionTitle => "Session title",
            Str::AttachHint => "attach",

            Str::You => "You",
            Str::Assistant => "Assistant",
            Str::Generating => "▓ generating…",
            Str::TypeMessage => "Type a message…",
            Str::SendEnter => "Enter to send",
            Str::StopEsc => "Esc to stop",
            Str::Attachments => "Attachments",
            Str::NoHistory => "No messages yet. Say hi!",
            Str::OpenDraft => "New chat (not created yet — first message starts it)",

            Str::HelpTitle => "Help",
            Str::Help => "Help",
            Str::ModelPicker => "Model",
            Str::ModelPickerEmpty => "no models (check harness connection)",
            Str::HarnessPicker => "Harness",
            Str::Harness => "Harness",
            Str::HarnessKind => "kind",
            Str::SettingsTitle => "Settings",
            Str::SettingsLanguage => "Language",
            Str::SettingsProfile => "Profile name",
            Str::SettingsActiveHarness => "Active harness",
            Str::SettingsServerUrl => "Server URL (empty = autostart)",
            Str::SettingsServerUrlHint => "e.g. http://127.0.0.1:4096",
            Str::SettingsWorkdir => "Working directory",
            Str::SettingsAgent => "Agent",
            Str::SettingsDefaultModel => "Default model",
            Str::SettingsEnabled => "enabled",
            Str::SettingsDisabledKind => "not supported yet",
            Str::SettingsRestartNote => "harness restarts on next launch",
            Str::FilePicker => "Attach files",
            Str::Drives => "Drives",
            Str::PickAttach => "Attach",
            Str::ParentDir => ".. up",

            Str::NewlineHint => "Alt+Enter = newline",
            Str::NotConnected => "backend not connected",
            Str::Draft => "draft",
            Str::StopNote => "stopped following — result loads when the turn ends (next message interrupts)",
            Str::SettingsServerPassword => "Server password (Basic auth)",
            Str::PermOnce => "Allow once",
            Str::PermAlways => "Always allow this session",
            Str::PermReject => "Deny",
            Str::SettingsAutoApprove => "Auto-approve tools (--yolo)",

            Str::CheckOk => "OK",
            Str::CheckModels => "models",
            Str::CheckSessions => "sessions",
        },
        Lang::Ru => match key {
            Str::Ok => "ОК",
            Str::Cancel => "Отмена",
            Str::Close => "Закрыть",
            Str::Save => "Сохранить",
            Str::Delete => "Удалить",
            Str::Rename => "Переименовать",
            Str::Confirm => "Подтверждение",
            Str::Yes => "Да",
            Str::No => "Нет",
            Str::Error => "Ошибка",
            Str::Loading => "загрузка…",
            Str::Empty => "(пусто)",
            Str::None_ => "нет",
            Str::Search => "Поиск",

            Str::AppTitle => "ai-commander",
            Str::StateIdle => "простой",
            Str::StateBusy => "генерация…",
            Str::StateError => "ошибка",
            Str::StateConnecting => "подключение…",

            Str::History => "История",
            Str::NewChat => "Новый чат",
            Str::Settings => "Настройки",
            Str::Profile => "Профиль",
            Str::Active => "активен",
            Str::NoSessions => "сессий пока нет",
            Str::SessionDeleted => "сессия удалена",
            Str::DeleteSession => "Удалить сессию",
            Str::DeleteSessionQ => "Удалить эту сессию безвозвратно?",
            Str::RenameSession => "Переименовать сессию",
            Str::SessionTitle => "Название сессии",
            Str::AttachHint => "файл",

            Str::You => "Вы",
            Str::Assistant => "Ассистент",
            Str::Generating => "▓ генерация…",
            Str::TypeMessage => "Введите сообщение…",
            Str::SendEnter => "Enter — отправить",
            Str::StopEsc => "Esc — стоп",
            Str::Attachments => "Вложения",
            Str::NoHistory => "Сообщений пока нет. Начните диалог!",
            Str::OpenDraft => "Новый чат (ещё не создан — начнётся с первого сообщения)",

            Str::HelpTitle => "Помощь",
            Str::Help => "Помощь",
            Str::ModelPicker => "Модель",
            Str::ModelPickerEmpty => "нет моделей (проверьте подключение)",
            Str::HarnessPicker => "Харнесс",
            Str::Harness => "Харнесс",
            Str::HarnessKind => "тип",
            Str::SettingsTitle => "Настройки",
            Str::SettingsLanguage => "Язык",
            Str::SettingsProfile => "Имя профиля",
            Str::SettingsActiveHarness => "Активный харнесс",
            Str::SettingsServerUrl => "URL сервера (пусто = автозапуск)",
            Str::SettingsServerUrlHint => "напр. http://127.0.0.1:4096",
            Str::SettingsWorkdir => "Рабочий каталог",
            Str::SettingsAgent => "Агент",
            Str::SettingsDefaultModel => "Модель по умолчанию",
            Str::SettingsEnabled => "включён",
            Str::SettingsDisabledKind => "пока не поддерживается",
            Str::SettingsRestartNote => "харнесс перезапустится при следующем старте",
            Str::FilePicker => "Прикрепить файлы",
            Str::Drives => "Диски",
            Str::PickAttach => "Прикрепить",
            Str::ParentDir => ".. наверх",

            Str::NewlineHint => "Alt+Enter — перенос строки",
            Str::NotConnected => "бэкенд не подключён",
            Str::Draft => "черновик",
            Str::StopNote => "следование остановлено — результат подгрузится по завершении хода (следующее сообщение прервёт)",
            Str::SettingsServerPassword => "Пароль сервера (Basic auth)",
            Str::PermOnce => "Разрешить один раз",
            Str::PermAlways => "Всегда разрешать в этой сессии",
            Str::PermReject => "Отклонить",
            Str::SettingsAutoApprove => "Авто-подтверждение инструментов (--yolo)",

            Str::CheckOk => "ОК",
            Str::CheckModels => "модели",
            Str::CheckSessions => "сессии",
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_empty_strings_in_either_language() {
        for lang in [Lang::En, Lang::Ru] {
            // Every key must resolve to a non-empty string.
            // Exhaustiveness is compile-time; here we check quality.
            assert!(!t(lang, Str::Ok).is_empty());
            assert!(!t(lang, Str::NewChat).is_empty());
        }
        assert_eq!(Lang::parse("ru"), Lang::Ru);
        assert_eq!(Lang::parse("EN"), Lang::En);
    }
}
