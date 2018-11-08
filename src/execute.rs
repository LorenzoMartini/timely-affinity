use timely::worker::Worker;
use timely_communication::Allocator;
use initialize::{Configuration, WorkerGuards};

pub fn execute_from_args<I, T, F>(iter: I, func: F) -> Result<WorkerGuards<T>,String>
    where I: Iterator<Item=String>,
          T:Send+'static,
          F: Fn(&mut Worker<Allocator>)->T+Send+Sync+'static, {
    let configuration = try!(Configuration::from_args(iter));
    execute(configuration, func)
}

pub fn execute<T, F>(mut config: Configuration, func: F) -> Result<WorkerGuards<T>,String>
where
    T:Send+'static,
    F: Fn(&mut Worker<Allocator>)->T+Send+Sync+'static {

    let mut core_ids = unsafe {
        let mut cpuset = ::std::mem::zeroed::<::libc::cpu_set_t>();
        ::libc::sched_getaffinity(0, ::std::mem::size_of::<::libc::cpu_set_t>(), &mut cpuset);
        let mut core_ids = Vec::new();
        for i in 0..::libc::CPU_SETSIZE as usize {
            if ::libc::CPU_ISSET(i, &cpuset) {
                core_ids.push(i);
            }
        }
        core_ids
    };

    let worker_core_ids = ::std::sync::Arc::new(match config {
        Configuration::Cluster { threads, ref mut spawn_fn, .. } => {
            let comm_core_ids = ::std::sync::Arc::new(core_ids.split_off(threads));

            *spawn_fn = Box::new(move |_, _, _, loop_fn| {
                unsafe {
                    let mut comm_cpuset = ::std::mem::zeroed::<::libc::cpu_set_t>();
                    eprintln!("comm core ids = {:?}", comm_core_ids);
                    ::libc::CPU_ZERO(&mut comm_cpuset);
                    for id in comm_core_ids.iter() {
                        ::libc::CPU_SET(*id, &mut comm_cpuset);
                    }

                    ::libc::sched_setaffinity(0, ::std::mem::size_of::<::libc::cpu_set_t>(), &comm_cpuset);
                }

                loop_fn.start()
            });

            eprintln!("core ids: {:?}", core_ids);
            
            core_ids
        },
        _ => core_ids,
    });

    timely_execute(config, move |worker| {
        let cpuid = worker_core_ids[worker.index() % worker_core_ids.len()];
        eprintln!("-> {} ({})", cpuid, worker.index() % worker_core_ids.len());
        unsafe {
            let mut cpuset = ::std::mem::zeroed::<::libc::cpu_set_t>();
            ::libc::CPU_ZERO(&mut cpuset);
            ::libc::CPU_SET(cpuid, &mut cpuset);
            ::libc::sched_setaffinity(0, ::std::mem::size_of::<::libc::cpu_set_t>(), &cpuset);
        }

        func(worker)
    })
}


fn timely_execute<T, F>(mut config: Configuration, func: F) -> Result<WorkerGuards<T>,String>
where
    T:Send+'static,
    F: Fn(&mut Worker<Allocator>)->T+Send+Sync+'static {

    if let Configuration::Cluster { ref mut log_fn, .. } = config {

        *log_fn = Box::new(|events_setup| {

            let mut result = None;
            if let Ok(addr) = ::std::env::var("TIMELY_COMM_LOG_ADDR") {

                use ::std::net::TcpStream;
                use timely::logging::BatchLogger;
                use timely::dataflow::operators::capture::EventWriter;

                eprintln!("enabled COMM logging to {}", addr);

                if let Ok(stream) = TcpStream::connect(&addr) {
                    let writer = EventWriter::new(stream);
                    let mut logger = BatchLogger::new(writer);
                    result = Some(::logging_core::Logger::new(
                        ::std::time::Instant::now(),
                        events_setup,
                        move |time, data| logger.publish_batch(time, data)
                    ));
                }
                else {
                    panic!("Could not connect to communication log address: {:?}", addr);
                }
            }
            result
        });
    }

    let (allocators, other) = config.try_build()?;

    ::initialize::initialize_from(allocators, other, move |allocator| {

        let mut worker = Worker::new(allocator);

        // If an environment variable is set, use it as the default timely logging.
        if let Ok(addr) = ::std::env::var("TIMELY_WORKER_LOG_ADDR") {

            use ::std::net::TcpStream;
            use timely::logging::{BatchLogger, TimelyEvent};
            use timely::dataflow::operators::capture::EventWriter;

            if let Ok(stream) = TcpStream::connect(&addr) {
                let writer = EventWriter::new(stream);
                let mut logger = BatchLogger::new(writer);
                worker.log_register()
                    .insert::<TimelyEvent,_>("timely", move |time, data|
                        logger.publish_batch(time, data)
                    );
            }
            else {
                panic!("Could not connect logging stream to: {:?}", addr);
            }
        }

        let result = func(&mut worker);
        while worker.step() { }
        result
    })
}
