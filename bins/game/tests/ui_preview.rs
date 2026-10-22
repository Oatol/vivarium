//! 把一帧界面离屏渲染成 PNG，供人眼检查。
//!
//! ## 为什么是测试，不是功能
//!
//! 像素类的问题——边框差一像素、字被裁掉、颜色脏、面板压住世界——
//! **写不出好断言**，但看一眼就知道。所以留一条"能把它导出成图片"的路，
//! 而不是在脑子里推演布局。
//!
//! 用测试而不是命令行开关，是因为：
//!
//! 1. `png` 只是 `dev-dependencies`，不会进发布产物（`dist/vivarium.exe` 6.8 MB
//!    里有 200 KB 是"给开发者看的截图工具"，不值）。
//! 2. 它需要 GPU，而 GPU 测试本来就要串行跑（见 README 里那条
//!    `STATUS_ACCESS_VIOLATION` 的坑）。
//!
//! 跑法：
//!
//! ```text
//! cargo test -p vivarium-game --test ui_preview -- --nocapture
//! ```
//!
//! 产物写在 `target/ui-preview.png`。

use vivarium_ui::state::{Level, SelectedOrganism, Snapshot, UiState};
use vivarium_ui::Ui;

const W: u32 = 1100;
const H: u32 = 840;

/// 一份**有内容**的假数据。
///
/// 全零的快照会让界面看起来"能用但空"——而空界面看不出排版问题：
/// 数值列不对齐、标签换行、长数字撑破一行，全都要有数据才暴露。
fn demo_snapshot() -> Snapshot {
    let mut census = [0usize; vivarium_core::MaterialKind::COUNT];
    census[vivarium_core::MaterialKind::Empty.as_u8() as usize] = 118_402;
    census[vivarium_core::MaterialKind::Stone.as_u8() as usize] = 84_216;
    census[vivarium_core::MaterialKind::Sand.as_u8() as usize] = 9_873;
    census[vivarium_core::MaterialKind::Water.as_u8() as usize] = 33_036;
    census[vivarium_core::MaterialKind::Gas.as_u8() as usize] = 412;
    census[vivarium_core::MaterialKind::Remains.as_u8() as usize] = 2_791;
    census[vivarium_core::MaterialKind::Corpse.as_u8() as usize] = 812;

    let mut cells = [0usize; vivarium_core::CellType::COUNT];
    cells[vivarium_core::CellType::Producer.as_u8() as usize] = 5;
    cells[vivarium_core::CellType::Mover.as_u8() as usize] = 4;
    cells[vivarium_core::CellType::Mouth.as_u8() as usize] = 1;
    cells[vivarium_core::CellType::Eye.as_u8() as usize] = 1;
    cells[vivarium_core::CellType::Killer.as_u8() as usize] = 1;

    Snapshot {
        tick: 28_431,
        fps: 61.4,
        ms_per_tick: 1.24,
        scale: vivarium_core::TimeScale::X5,
        world_size: (512, 512),
        pixels_per_cell: 0.9,
        active_chunks: (7, 64),
        organisms: 52,
        cells: 648,
        energy: 46_887,
        killers: 3,
        body_range: (10, 14),
        generation: 54,
        stranded: 4,
        food: 318,
        census,
        field: vivarium_core::FieldStats {
            sources: 3,
            avg_light: 0.42,
            max_light: 1.0,
            temp_range: (18.0, 620.0),
            avg_temp: 21.5,
        },
        // 悬停读数要能在预览图里看到，所以给一格"有生物"的数据。
        hover: Some(vivarium_ui::state::CellInfo {
            x: 271,
            y: 168,
            material: vivarium_core::MaterialKind::Empty,
            organism: Some(vivarium_ui::state::HoverOrganism {
                id: 4821,
                cells: 10,
                generation: 37,
                energy: (612, 1400),
                producers: 4,
                cell: vivarium_core::CellType::Producer,
            }),
            light: 0.71,
            temp: 22.0,
            depth: 19,
        }),
        history: (0..120)
            .map(|i| {
                let t = i as f32 / 12.0;
                (0.35 + 0.25 * t.sin() + 0.12 * (t * 2.7).sin()).clamp(0.0, 1.0)
            })
            .collect(),
        selected: Some(demo_selected(cells)),
    }
}

/// 一个"看起来像真的"的选中个体。
///
/// 形状、权重、走势都手写成一条银鱼的近似值——预览图要能验证
/// **缩略图画得像不像一条鱼**、**权重矩阵里那几行是不是亮的**，
/// 全零的占位数据在这两件事上什么都证明不了。
fn demo_selected(cells: [usize; vivarium_core::CellType::COUNT]) -> SelectedOrganism {
    use vivarium_core::CellType;
    // 银鱼：尾三格移动、前端两个嘴、背上两个生产者、腹下两个生产者、一只眼。
    let shape: Vec<(i16, i16, u8)> = [
        (CellType::Mover, -2, 0),
        (CellType::Mover, -1, 0),
        (CellType::Mover, 0, 0),
        (CellType::Mouth, 1, 0),
        (CellType::Mouth, 2, 0),
        (CellType::Producer, 1, -1),
        (CellType::Producer, 2, -1),
        (CellType::Producer, 0, 1),
        (CellType::Producer, 1, 1),
        (CellType::Eye, 2, 1),
    ]
    .iter()
    .map(|&(k, x, y)| (x, y, k.as_u8()))
    .collect();

    // 权重：只让"撞墙掉头"和"往前游"亮起来，其余留 0——
    // 这样矩阵里能看出"某几行是亮的"，而不是一片均匀的底色。
    let mut direct = vec![0.0f32; 17 * 6];
    direct[5 * 6] = 10.0; // 右被挡 → 左
    direct[4 * 6 + 1] = 10.0; // 左被挡 → 右
    direct[13 * 6 + 1] = 1.5; // 食物 x → 右
    direct[14 * 6 + 3] = 1.5; // 食物 y → 下
    direct[12 * 6 + 2] = -3.0; // 光照 → 上（负向抑制）
    direct[8 * 6 + 4] = 2.0; // 能量 → 吃

    let mut inputs = [0.0f32; 17];
    inputs[0] = 1.0; // 前方有食物
    inputs[8] = 0.62; // 能量
    inputs[9] = 0.06; // 年龄
    inputs[10] = 1.0; // 朝向 x
    inputs[12] = 0.70; // 光照
    inputs[13] = 0.35; // 食物在右前方
    inputs[14] = 0.55; // 偏下

    let outputs = [0.05f32, 0.71, -0.24, 0.42, 0.86, 0.12];

    SelectedOrganism {
        id: 42,
        anchor: (218, 341),
        energy: 312,
        max_energy: 3_500,
        age: (1_204, 20_000),
        speed: 1,
        smell: 24,
        upkeep: 37,
        move_cost: 13,
        mass: 13,
        cells,
        generation: 17,
        parent: Some(23),
        shape,
        energy_trace: (0..32)
            .map(|i| {
                let t = i as f32 / 8.0;
                (0.42 + 0.22 * t.sin() + 0.08 * (t * 3.1).cos()).clamp(0.0, 1.0)
            })
            .collect(),
        inputs,
        outputs,
        direct,
    }
}

/// 工坊模式：环境变量 `VIVARIUM_WORKSHOP=1` 时截的是工坊那一屏。
///
/// 用一个环境变量而不是两套测试：截图流程（建纹理、跑一帧、读回、
/// 写 PNG）有几十行，复制一份就意味着以后改一处要记得改两处。
fn workshop_mode() -> bool {
    std::env::var("VIVARIUM_WORKSHOP").is_ok()
}

/// 指针放哪儿：`VIVARIUM_HOVER=375,12`。
///
/// 有了它就能把"悬停某个按钮时会不会有东西挡住界面"变成一张图，
/// 而不用改代码再重跑——tooltip 这类东西**只有真的悬停才会出现**，
/// 而它恰恰是最容易挡住内容的。
fn hover_at() -> egui::Pos2 {
    let Some(spec) = std::env::var("VIVARIUM_HOVER").ok() else {
        return egui::pos2(520.0, 420.0);
    };
    let (x, y) = spec.split_once(',').unwrap_or(("520", "420"));
    egui::pos2(
        x.trim().parse().unwrap_or(520.0),
        y.trim().parse().unwrap_or(420.0),
    )
}

fn demo_state() -> UiState {
    let mut state = UiState::new();
    if workshop_mode() {
        state.workshop.open = true;
        state.workshop.name = "三节虫".to_owned();
        // 画一副 9 格的小鱼：中间一排移动 + 前端嘴 + 背上一排生产者。
        use vivarium_core::CellType;
        let body: &[(usize, usize, CellType)] = &[
            (7, 8, CellType::Producer),
            (8, 8, CellType::Producer),
            (9, 8, CellType::Producer),
            (7, 9, CellType::Mover),
            (8, 9, CellType::Mover),
            (9, 9, CellType::Mouth),
            (10, 9, CellType::Eye),
            (7, 10, CellType::Armor),
            (9, 10, CellType::Mover),
        ];
        for &(x, y, k) in body {
            state.workshop.set(x, y, Some(k));
        }
        state.library = vec![
            vivarium_ui::LibraryEntry {
                name: "银鱼".into(),
                cells: vec![(0, 0, 2), (1, 0, 0), (0, -1, 1)],
            },
            vivarium_ui::LibraryEntry {
                name: "三节虫".into(),
                cells: vec![(0, 0, 2), (1, 0, 2), (2, 0, 2)],
            },
        ];
    }
    state.log(0, Level::Info, "Vivarium 已启动");
    state.log(
        0,
        Level::Info,
        "中文字体已装载（C:\\Windows\\Fonts\\msyh.ttc）",
    );
    state.log(1_204, Level::Info, "投放 24 条");
    state.log(8_000, Level::Warn, "捕食者降到 1 枚——猎物密度可能不够");
    state.log(16_000, Level::Info, "残骸在 3.3% 附近收敛");
    state.log(24_000, Level::Error, "这一条是错误级别的示例，用来确认颜色");
    state
}

/// 有没有可用的 GPU。没有就跳过（CI 上没有显卡是常态）。
fn headless_device() -> Option<(wgpu::Device, wgpu::Queue)> {
    let instance = wgpu::Instance::default();
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        force_fallback_adapter: false,
        compatible_surface: None,
        apply_limit_buckets: false,
    }))
    .ok()?;
    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("ui-preview"),
        ..Default::default()
    }))
    .ok()?;
    Some((device, queue))
}

#[test]
fn ui_preview_png() {
    let Some((device, queue)) = headless_device() else {
        eprintln!("跳过：本机没有可用的 wgpu 适配器");
        return;
    };

    let format = wgpu::TextureFormat::Rgba8UnormSrgb;
    let mut ui = Ui::new();
    let mut state = demo_state();
    let snap = demo_snapshot();

    // --- 跑一帧界面 ---
    let input = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::pos2(0.0, 0.0),
            egui::vec2(W as f32, H as f32),
        )),
        // 指针放在中央，让"悬停"这一态也出现在图里。
        events: vec![egui::Event::PointerMoved(hover_at())],
        ..Default::default()
    };

    let (out, mut full) = ui.show(&mut state, &snap, input);
    assert!(out.repaint);

    let ppp = full.pixels_per_point;
    let paint_jobs = ui.ctx().tessellate(full.shapes, ppp);

    // --- 离屏目标 ---
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("ui-preview-target"),
        size: wgpu::Extent3d {
            width: W,
            height: H,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());

    let mut renderer = egui_wgpu::Renderer::new(
        &device,
        format,
        egui_wgpu::RendererOptions {
            dithering: false,
            ..Default::default()
        },
    );

    let screen = egui_wgpu::ScreenDescriptor {
        size_in_pixels: [W, H],
        pixels_per_point: ppp,
    };

    for (id, deltas) in &full.textures_delta.set {
        for delta in deltas {
            renderer.update_texture(&device, &queue, *id, delta);
        }
    }

    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("ui-preview"),
    });
    renderer.update_buffers(&device, &queue, &mut encoder, &paint_jobs, &screen);

    {
        let pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("ui-preview"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
                resolve_target: None,
                ops: wgpu::Operations {
                    // 真实运行时这里已经画好了世界；截图时清成背景色，
                    // 用来确认"面板和世界的对比度够不够"。
                    load: wgpu::LoadOp::Clear(wgpu::Color {
                        r: 0.02,
                        g: 0.05,
                        b: 0.09,
                        a: 1.0,
                    }),
                    store: wgpu::StoreOp::Store,
                },
                depth_slice: None,
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        let mut pass = pass.forget_lifetime();
        renderer.render(&mut pass, &paint_jobs, &screen);
    }

    for id in &full.textures_delta.free {
        renderer.free_texture(id);
    }
    // epaint 要求每个 `TexturesDelta` 都被"消费"掉，否则 drop 时 panic。
    // 真实运行时由 `egui-wgpu` 上传完就算消费；这里手动清。
    full.textures_delta.clear();

    // --- 读回 ---
    //
    // 每行必须按 256 字节对齐，所以缓冲宽度要向上取整。忘了这一步
    // 会得到一张斜着的图——这是 `copy_texture_to_buffer` 最经典的一次坑。
    let unpadded = W * 4;
    let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let padded = unpadded.div_ceil(align) * align;

    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("ui-preview-readback"),
        size: u64::from(padded) * u64::from(H),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });

    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded),
                rows_per_image: Some(H),
            },
        },
        wgpu::Extent3d {
            width: W,
            height: H,
            depth_or_array_layers: 1,
        },
    );

    queue.submit(Some(encoder.finish()));

    buffer.slice(..).map_async(wgpu::MapMode::Read, |r| {
        r.expect("读回失败");
    });
    device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("等待 GPU 失败");

    let data = buffer
        .slice(..)
        .get_mapped_range()
        .expect("映射读回缓冲失败");
    let mut pixels = Vec::with_capacity((unpadded * H) as usize);
    for row in 0..H {
        let start = (row * padded) as usize;
        pixels.extend_from_slice(&data[start..start + unpadded as usize]);
    }
    drop(data);
    buffer.unmap();

    // --- 写文件 ---
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("target")
        .join(if workshop_mode() {
            "workshop-preview.png"
        } else {
            "ui-preview.png"
        });
    let file = std::fs::File::create(&path).expect("创建 PNG 失败");
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), W, H);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header()
        .expect("写 PNG 头失败")
        .write_image_data(&pixels)
        .expect("写 PNG 数据失败");

    println!("界面截图：{}", path.display());

    // --- 像素探针 -----------------------------------------------------
    //
    // 这个模型看不了图，所以"看一眼"这件事必须换成断言。判据是
    // **几个必须成立的像素事实**，它们合起来等价于"面板都在、
    // 颜色都对、文字真的画出来了、中央留给了世界"。
    let px = |x: u32, y: u32| {
        let i = ((y * W + x) * 4) as usize;
        [pixels[i], pixels[i + 1], pixels[i + 2]]
    };
    let hex = |c: [u8; 3]| format!("#{:02x}{:02x}{:02x}", c[0], c[1], c[2]);

    // 工坊那一屏是**模态**：整窗被遮罩压暗、中间盖着一块面板，
    // 所以"顶栏是面板色""中央留给世界"这些断言必然不成立。
    // 这里只保留"画面上确实有一块比遮罩亮的区域"（= 面板真的画出来了），
    // 具体长什么样交给 PNG 本身。
    if workshop_mode() {
        let lit = pixels
            .chunks_exact(4)
            .filter(|p| p[0] > 24 || p[1] > 24 || p[2] > 24)
            .count();
        assert!(
            lit > (W * H / 4) as usize,
            "工坊面板应当占掉四分之一以上的画面，实际只有 {lit} 个亮像素"
        );
        println!("工坊截图：{}", path.display());
        return;
    }

    // 布局常量（与 `vivarium_ui::theme` 一致）。写死是有意的：
    // 布局变了这里会红，而那时候人本来就该重新看一眼图。
    let (top, strip, insp, pal, con) = (
        vivarium_ui::theme::TOP_BAR_H as u32,
        vivarium_ui::theme::TOOL_STRIP_W as u32,
        vivarium_ui::theme::INSPECTOR_W as u32,
        vivarium_ui::theme::PALETTE_H as u32,
        vivarium_ui::theme::CONSOLE_H as u32,
    );
    let panel = [
        vivarium_ui::theme::PANEL.r(),
        vivarium_ui::theme::PANEL.g(),
        vivarium_ui::theme::PANEL.b(),
    ];
    let sunken = [
        vivarium_ui::theme::SUNKEN.r(),
        vivarium_ui::theme::SUNKEN.g(),
        vivarium_ui::theme::SUNKEN.b(),
    ];

    // 1. 五块面板的底色**必须等于**主题里的那个值（每通道容差 1）。
    //
    //    容差不是偷懒：颜色要经过 sRGB 编码 → 写进纹理 → 解码读回，
    //    在最暗的那一段（线性 s/12.92）编码曲线很陡，一个 LSB 的
    //    舍入就会让 0x06 变成 0x07。这是量化误差，不是错。
    //
    //    而**真正的错**（多做一次 gamma 转换）会让 0x0b 变成 0x2a 那种
    //    量级的差——容差 1 照样抓得住。
    for (name, x, y, want) in [
        ("顶栏", 700, top / 2, panel),
        ("左栏", 8, 400, panel),
        ("右栏", W - 20, 400, panel),
        ("调色板", 500, H - con - pal / 2, panel),
        ("控制台", 500, H - 20, sunken),
    ] {
        let got = px(x, y);
        let close = (0..3).all(|i| got[i].abs_diff(want[i]) <= 1);
        assert!(
            close,
            "{name} ({x},{y}) 的底色不对：{} ≠ {}",
            hex(got),
            hex(want)
        );
    }

    // 1b. 左栏的**边界**要落在 `TOOL_STRIP_W` 那一列上。
    //
    //     比"左边是面板、中间是世界"更强：它钉住了交界的位置。
    //     差几像素在画面上表现为"世界被裁掉一条"，很容易被当成
    //     "相机夹得不好"去查渲染层。
    let inside = px(strip - 2, 400);
    let outside = px(strip + 2, 400);
    assert!(
        (0..3).all(|i| inside[i].abs_diff(panel[i]) <= 1),
        "左栏内侧应当是面板色：{}",
        hex(inside)
    );
    assert_ne!(outside, panel, "左栏外侧应当已经是世界：{}", hex(outside));

    // 2. 中央必须是**世界**，不是面板。
    //
    //    这条是布局的核心不变量：世界是主角，界面只是框。被面板盖住
    //    不会报任何错，画面却会变成"一堆面板"。
    let centre = px(W / 2, (top + (H - con - pal)) / 2);
    assert_ne!(centre, panel, "中央被面板盖住了：{}", hex(centre));

    // 3. 调色板里每种材质 / 细胞的色块都要**真的画出来**。
    //
    //    扫一整条带子找精确色值，而不是点在某个坐标上：色块的位置会
    //    随标签宽度变，钉死坐标会让这条测试变成"布局快照"而不是
    //    "颜色对不对"。
    let band_top = H - con - pal;
    let band_bottom = H - con;
    let mut found_material = [false; vivarium_core::MaterialKind::COUNT];
    let mut found_cell = [false; vivarium_core::CellType::COUNT];
    for y in band_top..band_bottom {
        for x in 0..W {
            let c = px(x, y);
            for k in vivarium_core::MaterialKind::ALL {
                if c == vivarium_core::palette::rgb(k) {
                    found_material[k.as_u8() as usize] = true;
                }
            }
            for k in vivarium_core::CellType::ALL {
                if c == vivarium_core::palette::cell_rgb(k) {
                    found_cell[k.as_u8() as usize] = true;
                }
            }
        }
    }
    for k in vivarium_core::MaterialKind::ALL {
        // 空材质在调色板里刻意不出现（画它没有意义：画空就是擦）。
        if k == vivarium_core::MaterialKind::Empty {
            continue;
        }
        assert!(
            found_material[k.as_u8() as usize],
            "调色板里找不到 {} 的色块",
            k.name()
        );
    }
    for k in vivarium_core::CellType::ALL {
        assert!(
            found_cell[k.as_u8() as usize],
            "调色板里找不到细胞 {k:?} 的色块"
        );
    }

    // 4. 文字必须**真的画出来了**。
    //
    //    判据是"检查器那一列里有足够多的亮像素"。字体没装上时中文会变成
    //    方块——方块也是亮像素，所以这条测不出"乱码"，但它能测出
    //    "整块面板是空的"（比如字体加载失败到什么都不画）。
    //    乱码由下面的字体断言兜住。
    let mut ink = 0usize;
    for y in top..(H - con - pal) {
        for x in (W - insp + 8)..(W - 8) {
            let c = px(x, y);
            let lum = u32::from(c[0]) + u32::from(c[1]) + u32::from(c[2]);
            if lum > 200 {
                ink += 1;
            }
        }
    }
    assert!(ink > 400, "检查器里几乎没有文字：只有 {ink} 个亮像素");

    // 4b. 缩略图里必须**真的有细胞色块**。
    //
    //     缩略图是本轮新增的东西里最容易"静默不画"的一个：它的数据来自
    //     `SelectedOrganism::shape`，主循环忘了填就是空 `Vec`，而空 `Vec`
    //     画出来只是"一个空的凹槽"——看起来像"这条鱼没有身体"，
    //     而不是"代码坏了"。
    //
    //     判据：检查器顶部那片区域里，至少能扫到 6 个**精确等于**某两种
    //     细胞配色的像素（银鱼有 3 格移动 + 4 格生产者，两种颜色各占一片）。
    let mut thumb = [0usize; vivarium_core::CellType::COUNT];
    for y in top..(top + 120) {
        for x in (W - insp + 8)..(W - 8) {
            let c = px(x, y);
            for k in vivarium_core::CellType::ALL {
                if c == vivarium_core::palette::cell_rgb(k) {
                    thumb[k.as_u8() as usize] += 1;
                }
            }
        }
    }
    assert!(
        thumb[vivarium_core::CellType::Mover.as_u8() as usize] > 4,
        "缩略图里没有移动细胞：{thumb:?}"
    );
    assert!(
        thumb[vivarium_core::CellType::Producer.as_u8() as usize] > 4,
        "缩略图里没有生产者细胞：{thumb:?}"
    );

    // 4c. 输入 / 输出那两栏必须画出了**彩色的条**。
    //
    //     条形用的是 `theme::INFO`（蓝）和 `theme::ACCENT`（绿）。
    //     这两种颜色在别处也会出现（强调色到处都是），所以判据放在
    //     "检查器这一列里 INFO 蓝的像素数"上：输入有 17 行，
    //     每行至少十几个像素宽，加起来不该只有个位数。
    let info = [
        vivarium_ui::theme::INFO.r(),
        vivarium_ui::theme::INFO.g(),
        vivarium_ui::theme::INFO.b(),
    ];
    let mut info_px = 0usize;
    for y in top..(H - con - pal) {
        for x in (W - insp + 8)..(W - 8) {
            let c = px(x, y);
            if (0..3).all(|i| c[i].abs_diff(info[i]) <= 2) {
                info_px += 1;
            }
        }
    }
    assert!(
        info_px > 120,
        "检查器里几乎没有输入条（INFO 蓝只有 {info_px} 个像素）"
    );

    // 4d. 权重矩阵必须**在可见区域里**画出来。
    //
    //     判据是红色像素：矩阵里正权重用强调绿、负权重用错误红，
    //     而界面里别处没有红色的大色块（只有错误日志的文字是红的，
    //     而那在控制台，不在检查器这一列）。示例基因组里恰好有一个负权重
    //     （光照 → 上，−3.0），它会画成一格红。
    //
    //     这一条同时钉住了**布局**：矩阵要是被挤到可视区外，
    //     像素就扫不到——而那正是"功能做了但看不见"的典型症状。
    let mut red = 0usize;
    for y in top..(H - con - pal) {
        for x in (W - insp + 8)..(W - 8) {
            let c = px(x, y);
            let (r, g, b) = (c[0] as i32, c[1] as i32, c[2] as i32);
            if r > 90 && r > g + 30 && r > b + 30 {
                red += 1;
            }
        }
    }
    assert!(
        red > 20,
        "检查器里找不到权重矩阵（红色像素只有 {red} 个）——它多半被挤到可视区外了"
    );

    // 5. 中文字体必须装上，否则界面上全是方块。
    //
    //    这条在"没有雅黑的机器"上会失败——那正是想要的：那时界面确实
    //    不可读，应该有人知道。
    assert!(ui.font.is_some(), "没找到中文字体，界面上的中文会变成方块");

    println!("界面截图：{}", path.display());
}
