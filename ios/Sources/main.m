    // iOS app shell.
//
// Two modes, picked at compile time:
//   CHATTER_RUST_UI defined  → hand off to the Rust side, which brings up
//                              Slint/winit (winit owns UIApplicationMain).
//   otherwise                → run chatter-core's smoke harness and render the
//                              report, so we can see the core working on device
//                              while the Slint iOS port is unfinished.
//
// the smoke half goes away with CHATTER_RUST_UI. Don't grow it.

#import <UIKit/UIKit.h>

extern char *chatter_ios_smoke(const char *docs_dir);
extern void chatter_ios_string_free(char *s);

#ifdef CHATTER_RUST_UI
extern void chatter_ios_run_ui(void);
#endif

@interface AppDelegate : UIResponder <UIApplicationDelegate>
@property(nonatomic, strong) UIWindow *window;
@end

@implementation AppDelegate

- (BOOL)application:(UIApplication *)application
    didFinishLaunchingWithOptions:(NSDictionary *)options {
  self.window = [[UIWindow alloc] initWithFrame:UIScreen.mainScreen.bounds];
  UIViewController *vc = [UIViewController new];
  vc.view.backgroundColor = UIColor.systemBackgroundColor;

  UITextView *out = [[UITextView alloc] initWithFrame:CGRectZero];
  out.editable = NO;
  out.font = [UIFont monospacedSystemFontOfSize:12 weight:UIFontWeightRegular];
  out.translatesAutoresizingMaskIntoConstraints = NO;
  [vc.view addSubview:out];
  UILayoutGuide *safe = vc.view.safeAreaLayoutGuide;
  [NSLayoutConstraint activateConstraints:@[
    [out.topAnchor constraintEqualToAnchor:safe.topAnchor constant:16],
    [out.bottomAnchor constraintEqualToAnchor:safe.bottomAnchor constant:-16],
    [out.leadingAnchor constraintEqualToAnchor:safe.leadingAnchor constant:16],
    [out.trailingAnchor constraintEqualToAnchor:safe.trailingAnchor constant:-16],
  ]];

  self.window.rootViewController = vc;
  [self.window makeKeyAndVisible];

  // Rust needs a writable dir; on iOS that's the sandbox Documents path, which
  // is far cheaper to fetch here than through a Foundation binding in Rust.
  NSString *docs = NSSearchPathForDirectoriesInDomains(NSDocumentDirectory,
                                                       NSUserDomainMask, YES)
                       .firstObject;

  // Off the main thread: the smoke run touches SQLCipher and the Keychain.
  dispatch_async(dispatch_get_global_queue(QOS_CLASS_USER_INITIATED, 0), ^{
    char *report = chatter_ios_smoke(docs.UTF8String);
    NSString *text = report ? @(report) : @"chatter_ios_smoke returned NULL";
    if (report) chatter_ios_string_free(report);
    NSLog(@"[chatter] %@", text);
    dispatch_async(dispatch_get_main_queue(), ^{ out.text = text; });
  });

  return YES;
}

@end

int main(int argc, char *argv[]) {
#ifdef CHATTER_RUST_UI
  @autoreleasepool {
    // An app bundle inherits no shell environment, so the server URL rides in
    // Info.plist (CHATTERServerURL, substituted from the CHATTER_SERVER_URL
    // build setting) and is re-exported here before any Rust code reads it.
    // Bootstrap plumbing only — chatter-core still owns what to do with it.
    NSString *url = NSBundle.mainBundle.infoDictionary[@"CHATTERServerURL"];
    if (url.length) {
      setenv("CHATTER_SERVER_URL", url.UTF8String, 1);
      NSLog(@"[chatter] server url %@", url);
    }
  }
  chatter_ios_run_ui();  // never returns
  return 0;
#else
  @autoreleasepool {
    return UIApplicationMain(argc, argv, nil,
                             NSStringFromClass(AppDelegate.class));
  }
#endif
}
