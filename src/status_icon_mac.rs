use std::cell::RefCell;
use std::sync::mpsc as channel;
use system_status_bar_macos::*;
use crate::send_stats::SendStats;


pub fn show_status_icon(receiver: channel::Receiver<SendStats>) {
    let (event_loop, terminator) = sync_event_loop(receiver, |_stat| {
        //let status_text = format!("Sent: {} bytes, Rate: {:.2} bps", stat.total_bytes_sent, stat.average_rate_bps);
        //status_item.borrow_mut().menu().
    });

    // Implementation for showing status icon on macOS
    let _status_item = RefCell::new(StatusItem::new("TITLE", Menu::new(vec![
      MenuItem::new("Stats", None, None),
      MenuItem::new("CLICKABLE MENU", Some(Box::new(move || {
        terminator.terminate();
        //click_sender.send(()).unwrap();
      })), None),
    ])));

    //sync_infinite_event_loop(receiver, |_| { });
    event_loop();
}
