use wgpu::{
    util::DeviceExt, ComputePassTimestampWrites, Features, InstanceFlags,
    QUERY_RESOLVE_BUFFER_ALIGNMENT,
};
use wgpu_test::{
    gpu_test, FailureCase, GpuTestConfiguration, GpuTestInitializer, TestParameters, TestingContext,
};

pub fn all_tests(vec: &mut Vec<GpuTestInitializer>) {
    vec.push(TIMESTAMP_QUERY);
    vec.push(TIMESTAMPS_SURVIVE_RESOLVE_IN_SAME_ENCODER);
}

const SHADER: &str = r#"
@compute @workgroup_size(1)
fn main() {
    return;
}
"#;

const ITERATIONS: u32 = 10;

const QUERIES_PER_ITERATION: u32 = 2;
const TOTAL_QUERIES: u32 = QUERIES_PER_ITERATION * ITERATIONS;

#[gpu_test]
static TIMESTAMP_QUERY: GpuTestConfiguration = GpuTestConfiguration::new()
    .parameters(
        TestParameters::default()
            .expect_fail(FailureCase::webgl2())
            .test_features_limits()
            .features(Features::TIMESTAMP_QUERY)
            // Ensure timestamp normalization functions correctly
            .instance_flags(InstanceFlags::AUTOMATIC_TIMESTAMP_NORMALIZATION),
    )
    .run_sync(timestamp_query);

fn timestamp_query(ctx: TestingContext) {
    // Setup pipeline using a simple shader with hardcoded vertices
    let shader = ctx
        .device
        .create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("timestamp query shader"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });

    let pipeline = ctx
        .device
        .create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("Pipeline"),
            layout: None,
            module: &shader,
            entry_point: None,
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            cache: None,
        });

    // Create timestamp query set
    let query_set = ctx.device.create_query_set(&wgpu::QuerySetDescriptor {
        label: Some("Query set"),
        ty: wgpu::QueryType::Timestamp,
        count: TOTAL_QUERIES,
    });

    let mut encoder = ctx
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());

    for i in 0..ITERATIONS {
        let base_index = i * QUERIES_PER_ITERATION;

        let mut compute_pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("compute pass"),
            timestamp_writes: Some(ComputePassTimestampWrites {
                query_set: &query_set,
                beginning_of_pass_write_index: Some(base_index),
                end_of_pass_write_index: Some(base_index + 1),
            }),
        });
        compute_pass.set_pipeline(&pipeline);

        compute_pass.dispatch_workgroups(1, 1, 1);
    }

    let buffer_size = QUERY_RESOLVE_BUFFER_ALIGNMENT * TOTAL_QUERIES as u64;
    let init_constant = 0x0123_4567_89AB_CDEFu64;

    let init_data = vec![init_constant; buffer_size as usize / 8];

    // Resolve query set to buffer
    let query_buffer = ctx
        .device
        .create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("Query buffer"),
            contents: bytemuck::cast_slice(&init_data),
            usage: wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC,
        });

    for i in 0..ITERATIONS {
        let start_query = i * QUERIES_PER_ITERATION;
        let end_query = start_query + QUERIES_PER_ITERATION;
        let buffer_offset = i as u64 * QUERY_RESOLVE_BUFFER_ALIGNMENT;

        encoder.resolve_query_set(
            &query_set,
            start_query..end_query,
            &query_buffer,
            buffer_offset,
        );
    }

    let mapping_buffer = ctx.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("Mapping buffer"),
        size: query_buffer.size(),
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    encoder.copy_buffer_to_buffer(&query_buffer, 0, &mapping_buffer, 0, query_buffer.size());

    ctx.queue.submit(Some(encoder.finish()));

    mapping_buffer
        .slice(..)
        .map_async(wgpu::MapMode::Read, |_| ());
    ctx.device
        .poll(wgpu::PollType::wait_indefinitely())
        .unwrap();
    let query_buffer_view = mapping_buffer.slice(..).get_mapped_range().unwrap();
    let query_data: &[u64] = bytemuck::cast_slice(&query_buffer_view);

    for i in 0..ITERATIONS {
        // The byte and query offset for the current iteration
        let byte_offset = i as u64 * QUERY_RESOLVE_BUFFER_ALIGNMENT;
        let query_offset = byte_offset / 8;

        // The byte and query offset for the next iteration
        let next_byte_offset = (i + 1) as u64 * QUERY_RESOLVE_BUFFER_ALIGNMENT;
        let next_query_offset = next_byte_offset / 8;

        // The range of queries that should still be the value they were initialized to.
        let untouched_query_start = query_offset + QUERIES_PER_ITERATION as u64;
        let untouched_query_end = next_query_offset;

        // WebGPU does not define the value of the timestamp queries. They unfortunately
        // can be `0` in some situations. However, we should expect that some value
        // has been written, and the odds of it being exactly `init_constant` are vanishingly low.
        for query in 0..QUERIES_PER_ITERATION {
            let query_index = query_offset + query as u64;
            assert_ne!(query_data[query_index as usize], init_constant);
        }

        // A pass cannot end before it began. Guarded on both ends carrying a value, because as
        // noted above a timestamp is allowed to come back as `0`, and comparing against one that
        // did would fail for reasons that have nothing to do with ordering.
        //
        // This catches a resolve that read a slot the GPU had not written yet and so returned a
        // stale value from an earlier submission. It will not catch the same fault surfacing as a
        // zero; see `timestamps_survive_resolve_in_same_encoder` below, which loads the passes
        // heavily enough to provoke that and checks for it directly.
        let pass_begin = query_data[query_offset as usize];
        let pass_end = query_data[query_offset as usize + 1];
        if pass_begin != 0 && pass_end != 0 {
            assert!(
                pass_end >= pass_begin,
                "iteration {i}: pass end timestamp {pass_end} precedes its begin timestamp \
                 {pass_begin}"
            );
        }

        // Validate that the queries that were not written to are still the value they were initialized to.
        for query in untouched_query_start..untouched_query_end {
            assert_eq!(query_data[query as usize], init_constant);
        }
    }
}

#[gpu_test]
static TIMESTAMPS_SURVIVE_RESOLVE_IN_SAME_ENCODER: GpuTestConfiguration =
    GpuTestConfiguration::new()
        .parameters(
            TestParameters::default()
                .expect_fail(FailureCase::webgl2())
                .test_features_limits()
                .features(Features::TIMESTAMP_QUERY),
        )
        .run_sync(timestamps_survive_resolve_in_same_encoder);

/// Shader with enough real work that the passes below take a measurable amount of time.
///
/// The write to `out` keeps the loop from being optimized away.
const LOADED_SHADER: &str = r#"
@group(0) @binding(0) var<storage, read_write> out: array<f32>;

@compute @workgroup_size(64)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    var acc = f32(gid.x) * 0.001;
    for (var i = 0u; i < 256u; i = i + 1u) {
        acc = fract(sin(acc * 12.9898 + f32(i)) * 43758.5453);
    }
    out[gid.x] = acc;
}
"#;

const LOADED_PASSES: u32 = 4;
const LOADED_WORKGROUPS: u32 = 2048;
const LOADED_ROUNDS: u32 = 8;

/// Regression test for the last timestamp written before a `resolve_query_set` in the same
/// encoder being lost.
///
/// On Apple GPUs from the Apple8 family onwards, counter write-back for timestamps sampled at
/// pass boundaries could still be in flight when `resolveCounters` ran, so the *final* timestamp
/// before the resolve came back as zero or as a stale value from a previous submit. The passes
/// have to do real work for this to show up, which is why [`TIMESTAMP_QUERY`] above - whose
/// compute passes are empty - did not catch it.
fn timestamps_survive_resolve_in_same_encoder(ctx: TestingContext) {
    let shader = ctx
        .device
        .create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("loaded shader"),
            source: wgpu::ShaderSource::Wgsl(LOADED_SHADER.into()),
        });
    let pipeline = ctx
        .device
        .create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("loaded pipeline"),
            layout: None,
            module: &shader,
            entry_point: None,
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            cache: None,
        });

    let storage = ctx.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("storage"),
        size: (LOADED_WORKGROUPS * 64) as u64 * 4,
        usage: wgpu::BufferUsages::STORAGE,
        mapped_at_creation: false,
    });
    let bind_group = ctx.device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("storage"),
        layout: &pipeline.get_bind_group_layout(0),
        entries: &[wgpu::BindGroupEntry {
            binding: 0,
            resource: storage.as_entire_binding(),
        }],
    });

    let query_count = LOADED_PASSES * 2;
    let query_set = ctx.device.create_query_set(&wgpu::QuerySetDescriptor {
        label: Some("timestamps"),
        ty: wgpu::QueryType::Timestamp,
        count: query_count,
    });
    let byte_size = query_count as u64 * 8;
    let resolve_buffer = ctx.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("resolve"),
        size: byte_size,
        usage: wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC,
        mapped_at_creation: false,
    });
    let mapping_buffer = ctx.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("mapping"),
        size: byte_size,
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });

    // Repeat, because a stale timestamp is only recognisable as such once there is a previous
    // value for it to be stale from, and because the race does not necessarily lose every time.
    for round in 0..LOADED_ROUNDS {
        let mut encoder = ctx
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());

        for pass_index in 0..LOADED_PASSES {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("loaded pass"),
                timestamp_writes: Some(ComputePassTimestampWrites {
                    query_set: &query_set,
                    beginning_of_pass_write_index: Some(pass_index * 2),
                    end_of_pass_write_index: Some(pass_index * 2 + 1),
                }),
            });
            pass.set_pipeline(&pipeline);
            pass.set_bind_group(0, &bind_group, &[]);
            pass.dispatch_workgroups(LOADED_WORKGROUPS, 1, 1);
        }

        encoder.resolve_query_set(&query_set, 0..query_count, &resolve_buffer, 0);
        encoder.copy_buffer_to_buffer(&resolve_buffer, 0, &mapping_buffer, 0, byte_size);
        ctx.queue.submit(Some(encoder.finish()));

        mapping_buffer
            .slice(..)
            .map_async(wgpu::MapMode::Read, |_| ());
        ctx.device
            .poll(wgpu::PollType::wait_indefinitely())
            .unwrap();
        let timestamps: Vec<u64> = {
            let view = mapping_buffer.slice(..).get_mapped_range().unwrap();
            bytemuck::cast_slice::<u8, u64>(&view).to_vec()
        };
        mapping_buffer.unmap();

        for pass_index in 0..LOADED_PASSES as usize {
            let begin = timestamps[pass_index * 2];
            let end = timestamps[pass_index * 2 + 1];
            assert!(
                begin != 0 && end != 0 && end > begin,
                "round {round}, pass {pass_index}: timestamps must be non-zero and increasing, \
                 got begin={begin} end={end} (all: {timestamps:?})"
            );
        }
    }
}
