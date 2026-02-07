                                                                                                           
  Phase breakdown (bottom-up, following ARCHITECTURE.md):                                                  
                                                                                                           
  1. Phase 1: Scaffolding - Create all module directories and stub files from ARCHITECTURE.md              
  2. Phase 2: Domain Layer - Core business entities (User, Message, Channel, etc.)                         
  3. Phase 3: Storage Layer - SQLCipher database, repositories, migrations                                 
  4. Phase 4: Crypto Layer - Signal Protocol with libsignal-client (sessions, prekeys, sender keys)        
  5. Phase 5: Network Layer - gRPC clients, connection management, offline queue                           
  6. Phase 6: Device Inbox - Polling, message handlers, validation pipeline                                
  7. Phase 7: Ephemeral Mailbox - NDSS '21 system (DM + group mailboxes, blind tokens)                     
  8. Phase 8: Services Layer - Business logic (auth, message, channel, user services)                      
  9. Phase 9: State Management - Reactive stores, event bus                                                
  10. Phase 10: System Integration - Tray, notifications, keyring                                          
  11. Phase 11: UI Foundation - Slint setup, theme, basic components                                       
  12. Phase 12: UI Features - Full chat UI, all screens and modals                                         
                                                                                                           
  Does this feel right? (yes / adjust)                                                                     
                                                                                                           
✻ Cooked for 43s                                                                                           
                                                                                                           
❯ device this into many more phases.                                                                       
                                                                                                           
● Here's a more granular breakdown following ARCHITECTURE.md structure:                                    
                                                                                                           
  Foundation                                                                                               
  1. Scaffolding - Create all directories and stub mod.rs files from ARCHITECTURE.md                       
                                                                                                           
  Domain Layer                                                                                             
  2. Domain: Core Entities - User, Message, Channel, Attachment (ids.rs, basic structs)                    
  3. Domain: Supporting Entities - Reaction, Notification, Call, Moderation                                
  4. Domain: Privacy Entities - Draft, Ephemeral, SavedMessage, Mention, Embed                             
                                                                                                           
  Storage Layer                                                                                            
  5. Storage: Database Setup - SQLCipher connection, migrations infrastructure                             
  6. Storage: Core Repositories - UserRepo, MessageRepo, ChannelRepo                                       
  7. Storage: Supporting Repositories - AttachmentRepo, ReactionRepo, DraftRepo, SettingsRepo              
  8. Storage: Search Index - FTS5 full-text search, search_index.rs                                        
  9. Storage: Key Store - Signal protocol key storage (identity, prekey, session)                          
                                                                                                           
  Crypto Layer                                                                                             
  10. Crypto: Signal Store - SignalProtocolStore trait implementation                                      
  11. Crypto: Identity & Sessions - Identity keys, session management                                      
  12. Crypto: Prekeys - PreKey bundles, signed prekeys, one-time prekeys                                   
  13. Crypto: Sender Keys - Group sender key distribution                                                  
  14. Crypto: Sealed Sender - Anonymous message sending, certificates                                      
  15. Crypto: Attachments - File encryption/decryption, key derivation                                     
                                                                                                           
  Network Layer                                                                                            
  16. Network: gRPC Foundation - Client pool, interceptors, connection state                               
  17. Network: Auth & User Clients - AuthClient, UserClient                                                
  18. Network: Channel & Message Clients - ChannelClient, MessageClient                                    
  19. Network: Supporting Clients - FileClient, NotificationClient, IntegrationClient, MailboxClient       
  20. Network: Streaming - StreamManager, message/presence/typing/reaction streams                         
  21. Network: Resilience - Connection recovery, retry logic, offline queue                                
  22. Network: Validation - Message orderer, deduplicator, validator pipeline                              
                                                                                                           
  Inbox & Mailbox                                                                                          
  23. Inbox: Provider & Poller - DeviceInboxProvider, polling infrastructure                               
  24. Inbox: Message Handlers - Chat, attachment, reaction, receipt, typing handlers                       
  25. Inbox: Sync Handlers - Key exchange, sender key, pin, sync handlers                                  
  26. Mailbox: Token Manager - Blind signature tokens, daily quota                                         
  27. Mailbox: DM System - Bootstrap, conversation keys, two-way anonymity                                 
  28. Mailbox: Group System - Member mailboxes, N-way fanout, coordination                                 
  29. Mailbox: Privacy Features - Cover traffic, key rolling, content encoding                             
                                                                                                           
  Services Layer                                                                                           
  30. Services: Auth - Login, logout, device auth, session management                                      
  31. Services: Account - Password change, account deletion, session list/revoke                           
  32. Services: Message Core - Send, receive, edit, delete messages                                        
  33. Services: Message Advanced - Forward, quote, pin, thread, draft                                      
  34. Services: Ephemeral Messages - TTL support (30s-24h), countdown timers                               
  35. Services: Channel - Create, join, leave, archive, settings, templates                                
  36. Services: User - Profile, status messages, friends, blocking                                         
  37. Services: Presence - Online/offline/away/DND tracking                                                
  38. Services: Real-time - Typing indicators, read receipts                                               
  39. Services: Reactions - Add/remove reactions, streaming                                                
  40. Services: Search - Global search, semantic search                                                    
  41. Services: Moderation - Ban, mute, kick, permissions                                                  
  42. Services: AI Integration - Server-side AI chat, summarization                                        
  43. Services: Saved Messages - Personal vault functionality                                              
                                                                                                           
  State & Events                                                                                           
  44. State: Core Stores - AppState, AuthState, UIState                                                    
  45. State: Chat Stores - ChatState, MessageState, PinnedState                                            
  46. State: User Stores - UserState, PresenceState, PrivacyState                                          
  47. State: Streaming & Offline - StreamingState, OfflineState                                            
  48. Events: Bus & Handlers - EventBus, domain events, UI events                                          
                                                                                                           
  System Integration                                                                                       
  49. System: Keyring & Directories - Secret Service, XDG directories                                      
  50. System: Tray & Notifications - StatusNotifierItem, notify-rust                                       
  51. System: Utilities - Clipboard, open URLs, autostart                                                  
                                                                                                           
  Media                                                                                                    
  52. Media: File Handling - File picker, MIME detection, thumbnails                                       
  53. Media: Playback - Audio player (rodio), video player                                                 
  54. Media: Recording - Voice recorder, video recorder                                                    
  55. Media: Previews - PDF viewer, code preview (syntax highlighting)                                     
  56. Media: GIF & Embeds - GIF picker, YouTube/link embeds                                                
                                                                                                           
  UI Layer                                                                                                 
  57. UI: Foundation - Slint setup, bridge, theme (colors, typography)                                     
  58. UI: Basic Components - Avatar, button, input, modal, toast, tooltip                                  
  59. UI: Pickers - Emoji picker, GIF picker, mention autocomplete                                         
  60. UI: Sidebar - Channel list, DM list, user profile, saved messages                                    
  61. UI: Chat Core - Message list, message item, message input                                            
  62. UI: Chat Features - Pinned bar, thread panel, reactions, typing indicator                            
  63. UI: File Components - Attachment tray, file preview, code preview, PDF viewer                        
  64. UI: Modals - Create channel, settings, profile, forward, invite                                      
  65. UI: Settings Screens - Account, privacy, encryption, appearance                                      
  66. UI: Auth Screens - Login, register, loading                                                          
                                                                                                           
  Does this feel right? (66 phases following the architecture)                                             
                                                                                                           
✻ Churned for 37s                                                                                          
                                                                                                           
❯ yes                                                                                                      
                                                                                                           
● Research needs detected:                                                                                 
  ┌───────┬──────────┬─────────────────────────────────────────────────┐                                   
  │ Phase │ Research │                     Reason                      │                                   
  ├───────┼──────────┼─────────────────────────────────────────────────┤                                   
  │ 1-4   │ Unlikely │ Internal Rust patterns, no external deps        │                                   
  ├───────┼──────────┼─────────────────────────────────────────────────┤                                   
  │ 5-9   │ Unlikely │ SQLCipher/rusqlite well-documented              │                                   
  ├───────┼──────────┼─────────────────────────────────────────────────┤                                   
  │ 10-15 │ Likely   │ libsignal-client API, Signal Protocol specifics │                                   
  ├───────┼──────────┼─────────────────────────────────────────────────┤                                   
  │ 16-22 │ Unlikely │ tonic/prost standard patterns                   │                                   
  ├───────┼──────────┼─────────────────────────────────────────────────┤                                   
  │ 23-29 │ Likely   │ NDSS '21 paper implementation details           │                                   
  ├───────┼──────────┼─────────────────────────────────────────────────┤                                   
  │ 30-43 │ Unlikely │ Internal business logic                         │                                   
  ├───────┼──────────┼─────────────────────────────────────────────────┤                                   
  │ 44-48 │ Unlikely │ Standard state management                       │                                   
  ├───────┼──────────┼─────────────────────────────────────────────────┤                                   
  │ 49-51 │ Unlikely │ ksni/notify-rust documented                     │                                   
  ├───────┼──────────┼─────────────────────────────────────────────────┤                                   
  │ 52-56 │ Unlikely │ rodio/rfd standard crates                       │                                   
  ├───────┼──────────┼─────────────────────────────────────────────────┤                                   
  │ 57-66 │ Likely   │ Slint API, component patterns                   │                                   
  └───────┴──────────┴─────────────────────────────────────────────────┘                                   
  Research topics for flagged phases:                                                                      
  - Phases 10-15 (Crypto): libsignal-client Rust API, X3DH flow, Double Ratchet, sender key protocol       
  - Phases 23-29 (Mailbox): NDSS '21 paper, blind signature implementation, cover traffic timing           
  - Phases 57-66 (UI): Slint component model, data binding, callbacks, styling                             
