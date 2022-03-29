use anyhow::Result;
use openrgb::{data::Color, OpenRGB};
use tokio::net::TcpStream;

pub async fn set_all_light_color(
    client: &OpenRGB<TcpStream>,
    cpu_usage: f32,
    gpu_usage: f32,
    start_color: &Color,
    end_color: &Color,
) -> Result<()> {
    let controller_count = client.get_controller_count().await?;
    for controller_id in 0..controller_count {
        let controller = client.get_controller(controller_id).await?;
        let led_count = controller.leds.len();
        let colors = match controller.name.as_str() {
            "ENE DRAM" => {
                generate_gradient_led_colors(1.0 - cpu_usage, end_color, start_color, led_count)
            }
            "EVGA GeForce RTX 3080Ti FTW3 Ultra" => {
                generate_block_led_colors(gpu_usage, start_color, end_color, led_count)
            }
            "X570 AORUS ELITE" => controller
                .zones
                .iter()
                .flat_map(|zone| match zone.name.as_str() {
                    "D_LED1 Bottom" => generate_block_led_colors(
                        cpu_usage,
                        start_color,
                        end_color,
                        zone.leds_count as usize,
                    ),
                    "D_LED2 Top" => generate_gradient_led_colors(
                        cpu_usage,
                        start_color,
                        end_color,
                        zone.leds_count as usize,
                    ),
                    "Motherboard" => {
                        let mut colors =
                            generate_block_led_colors(cpu_usage, start_color, end_color, 1);

                        colors.append(&mut generate_block_led_colors(
                            cpu_usage,
                            start_color,
                            end_color,
                            zone.leds_count as usize - 1,
                        ));

                        colors
                    }
                    _ => panic!("Unknown zone!"),
                })
                .collect(),
            _ => generate_block_led_colors(cpu_usage, start_color, end_color, led_count),
        };

        client.update_leds(controller_id, colors.clone()).await?;
    }

    Ok(())
}

fn lerp(value: f32, start: f32, end: f32) -> f32 {
    (1.0 - value) * start + (value * end)
}

fn lerp_color(value: f32, start_color: &Color, end_color: &Color) -> Color {
    Color::new(
        lerp(value, start_color.r as f32, end_color.r as f32) as u8,
        lerp(value, start_color.g as f32, end_color.g as f32) as u8,
        lerp(value, start_color.b as f32, end_color.b as f32) as u8,
    )
}

fn generate_gradient_led_colors(
    value: f32,
    start_color: &Color,
    end_color: &Color,
    size: usize,
) -> Vec<Color> {
    let value = value * size as f32;

    (0..size)
        .map(|index| {
            let value = value - index as f32;
            let value = value.clamp(0.0, 1.0);

            lerp_color(value, start_color, end_color)
        })
        .collect()
}

fn generate_block_led_colors(
    value: f32,
    start_color: &Color,
    end_color: &Color,
    size: usize,
) -> Vec<Color> {
    let color = lerp_color(value, start_color, end_color);

    vec![color; size]
}
