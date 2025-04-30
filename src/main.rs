use self::attachment::GeminiAttachment;
use anyhow::Result;
use futures_util::StreamExt;
use gemini_rs::types::{Content, Part, Role};
use regex::Regex;
use serenity::all::{
    ClientBuilder, Context, CreateAttachment, CreateMessage, EventHandler, GatewayIntents,
    HttpBuilder, Message, MessageType, Settings, UserId,
};
use serenity::async_trait;
use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::Duration;
use tokio::sync::Mutex;

mod attachment;
mod settings;

static REGEX_URL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\bhttps://\S+").unwrap());

struct Claide {
    gemini: gemini_rs::Client,
    seen: tokio::sync::Mutex<HashMap<String, GeminiAttachment>>,
    settings: settings::Settings,
    http_client: reqwest::Client,
}

impl Claide {
    async fn generate_response(&self, history: Vec<Content>) -> Result<String> {
        let mut chat = self
            .gemini
            .chat(&self.settings.gemini.model)
            .system_instruction(&self.settings.gemini.personality);
        *chat.history_mut() = history;

        let response = chat.generate_content().await?;
        Ok(format!("{response}"))
    }

    async fn process_message(&self, context: Context, message: Message) -> anyhow::Result<()> {
        let current_user_name = context.cache.current_user().display_name().to_string();
        let current_user_id = context.cache.current_user().id;

        if message.author.id == current_user_id {
            tracing::debug!("ignored self-message");

            return Ok(());
        }

        if !message.mentions_me(&context).await? {
            tracing::debug!("ignored non-mention");

            return Ok(());
        }

        let history = {
            let Some(cached_messages) = context.cache.channel_messages(message.channel_id) else {
                anyhow::bail!("no channel messages");
            };

            let mut messages = cached_messages.values().cloned().collect::<Vec<_>>();
            messages.sort_unstable_by(|a, b| a.id.cmp(&b.id));
            messages
                .into_iter()
                .filter(|msg| msg.kind == MessageType::Regular)
                .map(|msg| {
                    let role = if msg.author.id == current_user_id {
                        Role::Model
                    } else {
                        Role::User
                    };

                    let mut parts = Vec::new();
                    let (user_id, user_name) = if msg.author.id == current_user_id {
                        (current_user_id, current_user_name.to_string())
                    } else {
                        (msg.author.id, msg.author.display_name().to_string())
                    };
                    parts.push(Part::text(
                        format!("user(name={}, id={})", user_name, user_id).as_str(),
                    ));
                    parts.push(Part::text(&msg.content));

                    // let mut attachments = Vec::new();

                    // let content = &msg.content;
                    // attachments.extend(
                    //     REGEX_URL
                    //         .find_iter(content)
                    //         .map(|m| m.as_str())
                    //         .filter_map(|s| Url::try_from(s).ok())
                    //         .filter(|url| self.settings.gemini.whitelisted_domains.url_matches(url))
                    //         .map(Attachment::Url),
                    // );
                    // attachments.extend(
                    //     msg
                    //         .attachments
                    //         .iter()
                    //         .filter(|attachment| {
                    //             attachment
                    //                 .content_type
                    //                 .as_deref()
                    //                 .and_then(|content_type| content_type.parse::<Mime>().ok())
                    //                 .is_some_and(|mime| google_gemini::is_supported_mime(&mime))
                    //         })
                    //         .cloned()
                    //         .map(Attachment::Discord),
                    // );
                    // for (role, text, attachments) in previous_messages {
                    //     let attachment = attachments.into_iter().map(|attachment| async move {
                    //         anyhow::Ok(
                    //             match self.seen.lock().await.entry(attachment.url().to_string()) {
                    //                 Entry::Occupied(occupied) => occupied.get().clone(),
                    //                 Entry::Vacant(vacant) => {
                    //                     let pair = attachment.upload_into_gemini(self).await?;

                    //                     vacant.insert(pair.clone());

                    //                     pair
                    //                 }
                    //             },
                    //         )
                    //     });

                    //     let iter = futures_util::stream::iter(attachment)
                    //         .buffered(3)
                    //         .collect::<Vec<_>>()
                    //         .await
                    //         .into_iter()
                    //         .flatten()
                    //         .map(GeminiPart::from);

                    //     let mut parts = vec![GeminiPart::from(text)];

                    //     parts.extend(iter);

                    //     request.contents.push(GeminiMessage::new(role, parts));
                    // }

                    Content { role, parts }
                })
                .collect()
        };

        // request.system_instruction.parts.push(GeminiSystemPart {
        //     text: include_str!("personality.txt").into(),
        // });

        // let settings = [
        //     GeminiSafetySetting::HarmCategoryHarassment,
        //     GeminiSafetySetting::HarmCategoryHateSpeech,
        //     GeminiSafetySetting::HarmCategorySexuallyExplicit,
        //     GeminiSafetySetting::HarmCategoryDangerousContent,
        //     GeminiSafetySetting::HarmCategoryCivicIntegrity,
        // ];

        // let settings = settings.map(|setting| (setting)(GeminiSafetyThreshold::BlockNone));

        // request.safety_settings.extend(settings);

        let content = match self.generate_response(history).await {
            Ok(content) => content,
            Err(error) => {
                let mut builder = CreateMessage::new();
                builder = builder.content(format!("```\n{error}```\n-# repor issue to mari",));

                message.channel_id.send_message(&context, builder).await?;

                return Ok(());
            }
        };

        let content = content.trim();
        let content = content.strip_prefix("claide:").unwrap_or(content).trim();

        if content.is_empty() {
            anyhow::bail!("response is empty");
        }

        let mut builder = CreateMessage::new();

        if content.chars().count() > 1950 {
            builder = builder.add_file(CreateAttachment::bytes(content, "message.txt"));
        } else {
            builder = builder.content(content);
        }

        message.channel_id.send_message(&context, builder).await?;

        Ok(())
    }
}

#[async_trait]
impl EventHandler for Claide {
    async fn message(&self, context: Context, message: Message) {
        tracing::info!("new message");
        if let Err(error) = self.process_message(context, message).await {
            tracing::error!("process_message: {error:?}");
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let settings = settings::try_load()?;

    tracing_subscriber::fmt::init();

    let mut cache_settings = Settings::default();

    cache_settings.max_messages = settings.discord.cache_size;
    cache_settings.time_to_live = Duration::from_secs(24 * 60 * 60);

    let mut client = ClientBuilder::new(
        settings.discord.token.clone(),
        GatewayIntents::MESSAGE_CONTENT,
    )
    .cache_settings(cache_settings)
    .event_handler(Claide {
        gemini: gemini_rs::Client::new(settings.gemini.api_key.clone()),
        seen: Mutex::new(HashMap::new()),
        settings,
        http_client: reqwest::Client::new(),
    })
    .await?;

    client.start().await?;

    Ok(())
}
