// Copyright 2026 Ryan Daum
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::io::IsTerminal;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Generic table formatter with proper Unicode-aware column alignment
pub struct TableFormatter {
    headers: Vec<String>,
    column_widths: Vec<usize>,
    rows: Vec<Vec<String>>,
    column_alignments: Vec<Alignment>,
    group_split_after: Option<usize>,
    border_color: Option<BorderColor>,
}

#[derive(Clone, Copy)]
pub enum Alignment {
    Left,
    Right,
    Center,
}

#[derive(Clone, Copy)]
pub enum BorderColor {
    Cyan,
}

impl TableFormatter {
    pub fn new(headers: Vec<&str>, column_widths: Vec<usize>) -> Self {
        let alignments = (0..column_widths.len())
            .map(|i| {
                if i == 0 {
                    Alignment::Left
                } else {
                    Alignment::Right
                }
            })
            .collect();
        Self {
            headers: headers.iter().map(|s| s.to_string()).collect(),
            column_widths,
            rows: Vec::new(),
            column_alignments: alignments,
            group_split_after: None,
            border_color: None,
        }
    }

    pub fn with_alignments(mut self, alignments: Vec<Alignment>) -> Self {
        self.column_alignments = alignments;
        self
    }

    pub fn with_group_split_after(mut self, column: usize) -> Self {
        self.group_split_after = Some(column);
        self
    }

    pub fn with_border_color(mut self, color: BorderColor) -> Self {
        self.border_color = Some(color);
        self
    }

    pub fn add_row(&mut self, row: Vec<&str>) {
        self.rows.push(row.iter().map(|s| s.to_string()).collect());
    }

    fn format_cell(&self, text: &str, width: usize, align: Alignment) -> String {
        let display_width = display_text_width(text);
        if display_width > width {
            let truncated = truncate_display_text(text, width.saturating_sub(3));
            format!("{truncated}...")
        } else {
            let padding = width.saturating_sub(display_width);
            match align {
                Alignment::Left => format!("{}{}", text, " ".repeat(padding)),
                Alignment::Right => format!("{}{}", " ".repeat(padding), text),
                Alignment::Center => {
                    let left_pad = padding / 2;
                    let right_pad = padding - left_pad;
                    format!("{}{}{}", " ".repeat(left_pad), text, " ".repeat(right_pad))
                }
            }
        }
    }

    pub fn print(&self) {
        let has_headers = !self.headers.is_empty();

        // Top border
        print!("{}", self.borderize("┌"));
        for (i, &width) in self.column_widths.iter().enumerate() {
            print!("{}", self.borderize(&"─".repeat(width)));
            if i < self.column_widths.len() - 1 {
                print!("{}", self.borderize(self.vertical_top(i)));
            }
        }
        println!("{}", self.borderize("┐"));

        // Header row (only if headers exist)
        if has_headers {
            print!("{}", self.borderize("│"));
            for (i, (header, &width)) in self
                .headers
                .iter()
                .zip(self.column_widths.iter())
                .enumerate()
            {
                let formatted = self.format_cell(header, width, Alignment::Center);
                print!("{formatted}");
                print!("{}", self.borderize(self.vertical_body(i)));
            }
            println!();

            // Header separator
            print!("{}", self.borderize("╞"));
            for (i, &width) in self.column_widths.iter().enumerate() {
                print!("{}", self.borderize(&"═".repeat(width)));
                if i < self.column_widths.len() - 1 {
                    print!("{}", self.borderize(self.vertical_header(i)));
                }
            }
            println!("{}", self.borderize("╡"));
        }

        // Data rows with separators between them
        for row in &self.rows {
            print!("{}", self.borderize("│"));
            for (i, (cell, &width)) in row.iter().zip(self.column_widths.iter()).enumerate() {
                let align = self
                    .column_alignments
                    .get(i)
                    .copied()
                    .unwrap_or(Alignment::Right);
                let formatted = self.format_cell(cell, width, align);
                print!("{formatted}");
                print!("{}", self.borderize(self.vertical_body(i)));
            }
            println!();
        }

        // Bottom border
        print!("{}", self.borderize("└"));
        for (i, &width) in self.column_widths.iter().enumerate() {
            print!("{}", self.borderize(&"─".repeat(width)));
            if i < self.column_widths.len() - 1 {
                print!("{}", self.borderize(self.vertical_bottom(i)));
            }
        }
        println!("{}", self.borderize("┘"));
    }

    fn vertical_top(&self, column: usize) -> &'static str {
        if self.group_split_after == Some(column) {
            "╥"
        } else {
            "┬"
        }
    }

    fn vertical_header(&self, column: usize) -> &'static str {
        if self.group_split_after == Some(column) {
            "╪"
        } else {
            "╤"
        }
    }

    fn vertical_body(&self, column: usize) -> &'static str {
        if column + 1 == self.column_widths.len() {
            "│"
        } else if self.group_split_after == Some(column) {
            "║"
        } else {
            "│"
        }
    }

    fn vertical_bottom(&self, column: usize) -> &'static str {
        if self.group_split_after == Some(column) {
            "╨"
        } else {
            "┴"
        }
    }

    fn borderize(&self, text: &str) -> String {
        if !std::io::stdout().is_terminal() {
            return text.to_string();
        }
        let Some(color) = self.border_color else {
            return text.to_string();
        };
        let code = match color {
            BorderColor::Cyan => "36",
        };
        format!("\x1b[{code}m{text}\x1b[0m")
    }
}

fn display_text_width(text: &str) -> usize {
    strip_ansi(text).width()
}

fn truncate_display_text(text: &str, target_width: usize) -> String {
    let mut output = String::new();
    let mut chars = text.chars().peekable();
    let mut width = 0;

    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            output.push(ch);
            for next in chars.by_ref() {
                output.push(next);
                if next == 'm' {
                    break;
                }
            }
            continue;
        }

        let ch_width = ch.width().unwrap_or(0);
        if width + ch_width > target_width {
            break;
        }
        width += ch_width;
        output.push(ch);
    }

    output
}

fn strip_ansi(text: &str) -> String {
    let mut output = String::new();
    let mut chars = text.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            for next in chars.by_ref() {
                if next == 'm' {
                    break;
                }
            }
            continue;
        }
        output.push(ch);
    }

    output
}
