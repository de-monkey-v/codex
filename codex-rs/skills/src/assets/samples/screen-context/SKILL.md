---
name: screen-context
description: Allows you to view the user's screen. Use when the user makes a reference to their recent work, for which it'd be helpful to see the screen. This skill MUST be used whenever you need to resolve ambiguity in a user request, where the user hasn't specified enough context to do the task. Examples include disambiguating the specific user/app/document/error the user is referring to.
---

# Screen Context

This skill allows you to view the user's screen. This skill is enabled because the user has enabled the Codex screen recording feature, which records a rolling buffer of the past 6 hours of work to `$CODEX_HOME/recording/screen_ephemeral`.

## File structure

```
$CODEX_HOME/recording/screen_ephemeral/
 ├── <utc_timestamp>-display-<display_id>-latest.jpg - latest frame for this segment + display
 ├── <utc_timestamp>-display-<display_id>.mp4 - mp4 file of the screen recording for this display (1 fps)
 └── <utc_timestamp>-display-<display_id>.mp4.json - metadata for this segment, contains the timestamp of the segment and the display ID but not any app information.
```

## Usage

The most common workflow is to read the latest frame of the screen recording for a given display, which represents the user's most recent work. Copy it to a temp file when you want to do file operations on it, because otherwise the file will be silently updated by the screen recording service.

Screen data should be used to get context on the user's work, but you must upgrade to other data sources (such as your app-specific skills, connectors, or the file system) as soon as you've gotten the minimum necessary context from the screenshot to do so. This is because your multimodal understanding is not that good, so you should avoid relying on it for complex tasks.

For example, if the user asks you to "review the doc I have open", you should view the context, see that e.g. it's a Google Doc with a doc ID, extract the doc ID, and then use the Google Doc connector to review the doc. You must not try to OCR the entire document from the screenshot (also because the user's screen may not show the entire content of the document).