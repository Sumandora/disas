use anyhow::anyhow;
use clap::Parser;
use kust::ScopeFunctions;
use mktemp::Temp;
use object::elf;
use object::read::elf::FileHeader;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::Stylize;
use ratatui::widgets::{Block, Borders};
use ratatui::{Terminal, crossterm};
use std::cell::OnceCell;
use std::io::{self};
use std::process::Stdio;
use tui_textarea::{Input, Key, TextArea};
use zydis::{Decoder, Formatter, VisibleOperands};

const INTEL_SYNTAX: &str = ".intel_syntax noprefix\n";

#[derive(Parser, Debug)]
struct Args {
    #[arg(short, long, default_value_t = false)]
    att_syntax: bool,

    #[arg(long, default_value_t = false)]
    m32: bool,
}

thread_local! {
static ARGS: OnceCell<Args> = const { OnceCell::new() };
}

fn assemble(code: String) -> Result<String, anyhow::Error> {
    if code.trim().is_empty() {
        return Ok(String::new());
    }

    let input_file = Temp::new_file()?;

    let mut content = code.clone();

    if !ARGS.with(|x| x.get().unwrap().att_syntax) {
        content = INTEL_SYNTAX.to_owned() + &content;
    }

    std::fs::write(&input_file, content)?;

    let i = input_file
        .to_str()
        .ok_or(anyhow!("input file isn't utf8"))?;

    let object_file = Temp::new_file()?;
    let o = object_file
        .to_str()
        .ok_or(anyhow!("object file isn't utf8"))?;

    let assembler = std::process::Command::new("as")
        .apply(|a| {
            if ARGS.with(|x| x.get().unwrap().m32) {
                a.arg("--32");
            }
        })
        .arg(i)
        .arg("-o")
        .arg(o)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null())
        .spawn()?;

    let output = assembler.wait_with_output()?;

    if !std::fs::exists(o)? {
        return Err(anyhow!(format!(
            "Assembler error:\n{}\n\n{}",
            String::from_utf8(output.stdout)?,
            String::from_utf8(output.stderr)?
        )));
    }

    let linked_file = Temp::new_file()?;

    let l = linked_file
        .to_str()
        .ok_or(anyhow!("linked file isn't utf8"))?;

    let linker = std::process::Command::new("ld")
        .apply(|a| {
            if ARGS.with(|x| x.get().unwrap().m32) {
                a.arg("-m").arg("elf_i386");
            }
        })
        .arg(o)
        .arg("-o")
        .arg(l)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null())
        .spawn()?;

    let output = linker.wait_with_output()?;

    if !std::fs::exists(l)? {
        return Err(anyhow!(format!(
            "Linker error:\n{}\n\n{}",
            String::from_utf8(output.stdout)?,
            String::from_utf8(output.stderr)?
        )));
    }

    let data = std::fs::read(l)?;

    let text_data = if !ARGS.with(|x| x.get().unwrap().m32) {
        let elf = elf::FileHeader64::<object::Endianness>::parse(&*data)?;
        let endian = elf.endian()?;
        let sections = elf.sections(endian, &*data)?;

        let text = sections
            .section_by_name(endian, ".text".as_bytes())
            .ok_or(anyhow::anyhow!("no text section"))?;

        &data[text.1.sh_offset.get(endian) as usize
            ..(text.1.sh_offset.get(endian) + text.1.sh_size.get(endian)) as usize]
    } else {
        let elf = elf::FileHeader32::<object::Endianness>::parse(&*data)?;
        let endian = elf.endian()?;
        let sections = elf.sections(endian, &*data)?;

        let text = sections
            .section_by_name(endian, ".text".as_bytes())
            .ok_or(anyhow::anyhow!("no text section"))?;

        &data[text.1.sh_offset.get(endian) as usize
            ..(text.1.sh_offset.get(endian) + text.1.sh_size.get(endian)) as usize]
    };
    Ok(text_data
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect::<Vec<_>>()
        .join(" "))
}

fn disassemble(code: String) -> Result<String, anyhow::Error> {
    if code.trim().is_empty() {
        return Ok(String::new());
    }

    let bytes = code.split_whitespace().map(|s| u8::from_str_radix(s, 16));

    let mut vec = Vec::new();

    for byte in bytes {
        vec.push(byte?);
    }

    let fmt = if ARGS.with(|x| x.get().unwrap().att_syntax) {
        Formatter::att()
    } else {
        Formatter::intel()
    };
    let dec = if ARGS.with(|x| x.get().unwrap().m32) {
        Decoder::new32()
    } else {
        Decoder::new64()
    };

    let mut str = String::new();

    for insn_info in dec.decode_all::<VisibleOperands>(&vec, 0) {
        let (ip, _raw_bytes, insn) = insn_info?;

        str = format!("{}{}\n", str, &fmt.format(Some(ip), &insn)?);
    }

    Ok(str)
}

fn main() -> io::Result<()> {
    let args = Args::parse();
    ARGS.with(|x| x.set(args)).unwrap();

    let stdout = io::stdout();
    let mut stdout = stdout.lock();

    enable_raw_mode()?;
    crossterm::execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut term = Terminal::new(backend)?;

    let mut assembler = TextArea::default();
    assembler.set_block(Block::default().borders(Borders::ALL).title("Assembler"));
    let mut disassembler = TextArea::default();
    disassembler.set_block(Block::default().borders(Borders::ALL).title("Disassembler"));

    #[derive(Clone, Copy)]
    enum SelectedWindow {
        Assembler,
        Disassembler,
    }

    impl SelectedWindow {
        fn to_mut_textarea<'a, 'b>(
            self,
            assembler: &'b mut TextArea<'a>,
            disassembler: &'b mut TextArea<'a>,
        ) -> (&'b mut TextArea<'a>, &'b mut TextArea<'a>) {
            match self {
                SelectedWindow::Assembler => (assembler, disassembler),
                SelectedWindow::Disassembler => (disassembler, assembler),
            }
        }

        fn other(self) -> SelectedWindow {
            match self {
                SelectedWindow::Assembler => SelectedWindow::Disassembler,
                SelectedWindow::Disassembler => SelectedWindow::Assembler,
            }
        }
    }

    let mut selection = SelectedWindow::Assembler;

    loop {
        {
            let (curr_textarea, other_textarea) =
                selection.to_mut_textarea(&mut assembler, &mut disassembler);

            curr_textarea.set_style(curr_textarea.style().white());
            other_textarea.set_style(other_textarea.style().gray());
        }

        term.draw(|f| {
            let lay = Layout::horizontal(Constraint::from_percentages([50, 50]));
            let [left, right] = lay.areas(f.area());
            f.render_widget(&assembler, left);
            f.render_widget(&disassembler, right);
        })?;

        let (curr_textarea, _) = selection.to_mut_textarea(&mut assembler, &mut disassembler);

        match crossterm::event::read()?.into() {
            Input { key: Key::Esc, .. } => break,
            Input {
                key: Key::Char('q'),
                ctrl: true,
                ..
            } => break,
            Input {
                key: Key::Left,
                ctrl: true,
                alt: false,
                shift: true,
            } => selection = selection.other(),
            Input {
                key: Key::Right,
                ctrl: true,
                alt: false,
                shift: true,
            } => selection = selection.other(),
            input => {
                if curr_textarea.input(input) {
                    match selection {
                        SelectedWindow::Assembler => {
                            let str = match assemble(assembler.lines().join("\n")) {
                                Ok(str) => str,
                                Err(err) => err.to_string(),
                            };

                            disassembler.select_all();
                            disassembler.delete_char();
                            disassembler.set_yank_text(str);
                            disassembler.paste();
                            disassembler.set_yank_text(String::new());
                        }
                        SelectedWindow::Disassembler => {
                            let str = match disassemble(disassembler.lines().join("\n")) {
                                Ok(str) => str,
                                Err(err) => err.to_string(),
                            };

                            assembler.select_all();
                            assembler.delete_char();
                            assembler.set_yank_text(str);
                            assembler.paste();
                            assembler.set_yank_text(String::new());
                        }
                    }
                }
            }
        }
    }

    disable_raw_mode()?;
    crossterm::execute!(
        term.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    term.show_cursor()?;

    println!("Lines: {:?}", assembler.lines());
    Ok(())
}
